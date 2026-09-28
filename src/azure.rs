use crate::AppSettings;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oauth2::basic::{BasicClient as Oauth2BasicClient, BasicTokenResponse};
use oauth2::{AuthUrl, EndpointNotSet, EndpointSet, Scope, TokenResponse, TokenUrl};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::{Registry, Unit};

use crate::exporter::{LabelValue, PromScraper, UpstreamHttpError};
use reqwest::{Client as HttpClient, Response, StatusCode};
use serde::Deserialize;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};
use tracing::{debug, info, warn};

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

static AZURE_BASE_URL: &str = "https://login.microsoftonline.com";
static AZURE_AUTH_PATH: &str = "oauth2/v2.0/authorize";
static AZURE_TOKEN_PATH: &str = "oauth2/v2.0/token";
static AZURE_SCOPE: &str = "https://graph.microsoft.com/.default";
static AZURE_APPLICATIONS_ENDPOINT: &str = "https://graph.microsoft.com/v1.0/applications/";
static AZURE_TOKEN_MIN_LIFETIME: u64 = 60;
static AZURE_TOKEN_FETCH_RETRY: u64 = 10;
/// Unparseable error bodies are logged verbatim, up to this many characters.
static GRAPH_ERROR_BODY_MAX_CHARS: usize = 1024;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Credentials {
    // custom_key_identifier: Option<String>,
    display_name: Option<String>,
    end_date_time: DateTime<Utc>,
    // hint: Option<String>,
    key_id: String,
    // start_date_time: DateTime<Utc>,
}

impl Display for Credentials {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let display_name = self
            .display_name
            .as_ref()
            .map_or_else(String::new, |v| format!(" ({})", v));
        write!(f, "{}{}: {}", self.key_id, display_name, self.end_date_time)
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct AzureApp {
    app_id: String,
    display_name: String,
    password_credentials: Vec<Credentials>,
    key_credentials: Vec<Credentials>,
}

impl Display for AzureApp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut result = format!("{} ({}):", self.display_name, self.app_id);

        result.push_str("\n\tPassword Credentials:");
        if self.password_credentials.is_empty() {
            result.push_str(" None");
        } else {
            for cred in &self.password_credentials {
                result.push_str("\n\t\t");
                result.push_str(&cred.to_string());
            }
        }

        result.push_str("\n\tKey Credentials:");
        if self.key_credentials.is_empty() {
            result.push_str(" None");
        } else {
            for cred in &self.key_credentials {
                result.push_str("\n\t\t");
                result.push_str(&cred.to_string());
            }
        }

        write!(f, "{}", result)
    }
}

/// Error body returned by Microsoft Graph on failed requests.
#[derive(Deserialize, Debug)]
struct GraphErrorResponse {
    error: GraphError,
}

#[derive(Deserialize, Debug)]
struct GraphError {
    code: String,
    message: String,
}

/// Turn a failed Graph response into an error that says why it failed.
///
/// `error_for_status` only keeps the status code, but the body carries Graph's
/// error code and message, and the `request-id` header is what Microsoft
/// support asks for.
async fn graph_error(response: Response) -> anyhow::Error {
    let status = response.status();
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let message = match response.text().await {
        Ok(body) => describe_graph_error(status, &body),
        Err(err) => format!("Graph returned {status} and its body could not be read: {err}"),
    };
    UpstreamHttpError {
        status: status.as_u16(),
        request_id,
        message,
    }
    .into()
}

fn describe_graph_error(status: StatusCode, body: &str) -> String {
    let detail = match serde_json::from_str::<GraphErrorResponse>(body) {
        Ok(GraphErrorResponse { error }) => format!("{}: {}", error.code, error.message),
        Err(_) => body.chars().take(GRAPH_ERROR_BODY_MAX_CHARS).collect(),
    };
    format!("Graph returned {status}: {detail}")
}

#[derive(Deserialize, Debug)]
struct ResponsePage {
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    value: Vec<AzureApp>,
}

struct Token {
    token_response: BasicTokenResponse,
    expires_at: Instant,
}

/// `BasicClient` tracks which endpoints are configured in its type.
/// Only the auth and token URIs are set here.
type AzureOauth2Client =
    Oauth2BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

pub struct AzureClientTokenProvider {
    oauth2_client: AzureOauth2Client,
    oauth2_http_client: HttpClient,
    token: RwLock<Option<Token>>,
}

impl AzureClientTokenProvider {
    pub fn init(settings: &AppSettings) -> Result<Self> {
        let auth_url = AuthUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, settings.azure_tenant_id, AZURE_AUTH_PATH
        ))?;
        let token_url = TokenUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, settings.azure_tenant_id, AZURE_TOKEN_PATH
        ))?;
        let oauth2_client = Oauth2BasicClient::new(settings.azure_client_id.to_owned())
            .set_client_secret(settings.azure_client_secret.to_owned())
            .set_auth_uri(auth_url)
            .set_token_uri(token_url);

        // The token endpoint must not follow redirects, so that the credentials
        // are never replayed against another host.
        let oauth2_http_client = HttpClient::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            oauth2_client,
            oauth2_http_client,
            token: RwLock::new(None),
        })
    }

    async fn refresh(&self) -> Result<Instant> {
        let result = self
            .oauth2_client
            .exchange_client_credentials()
            .add_scope(Scope::new(AZURE_SCOPE.to_string()))
            .request_async(&self.oauth2_http_client)
            .await
            .context("Failed to retrieve Azure token");

        match result {
            Err(err) => {
                *self.token.write().await = None;
                Err(err)
            }
            Ok(token_response) => {
                let expires_in = Duration::from_secs(
                    token_response
                        .expires_in()
                        .ok_or_else(|| anyhow!("Token doesn't have expiration date"))?
                        .as_secs(),
                );
                let expires_at =
                    Instant::now() + expires_in - Duration::from_secs(AZURE_TOKEN_MIN_LIFETIME);
                info!(
                    action = "token-refresh",
                    outcome = "success",
                    expires_in_secs = expires_in.as_secs(),
                    "Azure token refreshed"
                );
                *self.token.write().await = Some(Token {
                    token_response,
                    expires_at,
                });
                Ok(expires_at)
            }
        }
    }

    pub async fn work_cache(&self) {
        loop {
            let deadline = self.refresh().await.unwrap_or_else(|err| {
                warn!(
                    action = "token-refresh",
                    outcome = "failure",
                    error = %format!("{err:#}"),
                    retry_in_secs = AZURE_TOKEN_FETCH_RETRY,
                    "Failed to refresh Azure token"
                );
                Instant::now() + Duration::from_secs(AZURE_TOKEN_FETCH_RETRY)
            });

            tokio::time::sleep_until(deadline).await;
        }
    }

    pub async fn get_secret(&self) -> Result<String> {
        match self
            .token
            .read()
            .await
            .as_ref()
            .filter(|t| t.expires_at > Instant::now())
        {
            Some(token) => Ok(token.token_response.access_token().secret().clone()),
            None => Err(anyhow!("No Azure token available")),
        }
    }
}

pub struct AzureGraphClient {
    token_provider: Arc<AzureClientTokenProvider>,
    http_client: HttpClient,
    applications_endpoint: String,
}

impl AzureGraphClient {
    pub fn with_token_provider(token_provider: Arc<AzureClientTokenProvider>) -> Result<Self> {
        Self::new(
            token_provider,
            AZURE_APPLICATIONS_ENDPOINT.to_string(),
            true,
        )
    }

    /// `https_only` is only turned off by tests, which talk to a local mock.
    fn new(
        token_provider: Arc<AzureClientTokenProvider>,
        applications_endpoint: String,
        https_only: bool,
    ) -> Result<Self> {
        let http_client = HttpClient::builder()
            .user_agent(APP_USER_AGENT)
            .gzip(true)
            .timeout(Duration::from_secs(5))
            .https_only(https_only)
            .build()?;

        Ok(Self {
            http_client,
            token_provider,
            applications_endpoint,
        })
    }
}

#[async_trait]
impl PromScraper for AzureGraphClient {
    async fn scrape(&self) -> Result<Registry> {
        let mut registry = <Registry>::default();
        let credentials_metric = Family::<CredentialLabels, Gauge<u64, AtomicU64>>::default();
        registry.register_with_unit(
            "credential_expiration_time",
            "Timestamp of credential expiration",
            Unit::Seconds,
            credentials_metric.clone(),
        );

        let mut url = self.applications_endpoint.clone();
        let mut query = &[(
            "$select",
            "appId,displayName,keyCredentials,passwordCredentials",
        )];

        let mut page: u32 = 0;
        loop {
            page += 1;
            let response = self
                .http_client
                .get(url)
                .query(query)
                .bearer_auth(self.token_provider.get_secret().await?)
                .send()
                .await
                .with_context(|| format!("Graph request for page {page} failed"))?;
            if !response.status().is_success() {
                return Err(graph_error(response).await);
            }

            let body = response
                .json::<ResponsePage>()
                .await
                .with_context(|| format!("Failed to decode Graph response page {page}"))?;
            debug!(
                page,
                apps = body.value.len(),
                "Fetched Graph applications page"
            );
            for app in body.value {
                for credential in app
                    .password_credentials
                    .iter()
                    .chain(app.key_credentials.iter())
                {
                    credentials_metric
                        .get_or_create(&CredentialLabels {
                            app_name: app.display_name.as_str().into(),
                            app_id: app.app_id.as_str().into(),
                            key_id: credential.key_id.as_str().into(),
                        })
                        .set(credential.end_date_time.timestamp() as u64);
                }
            }

            if let Some(next_link) = body.next_link {
                url = next_link.clone();
                query = &[("", "")];
            } else {
                break;
            }
        }

        Ok(registry)
    }

    async fn ready(&self) -> std::result::Result<String, String> {
        self.token_provider
            .get_secret()
            .await
            .map(|_| String::from("Ok"))
            .map_err(|e| format!("Unavailable: {}", e))
    }

    fn name(&self) -> &str {
        "Azure App Secrets"
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CredentialLabels {
    app_id: LabelValue,
    app_name: LabelValue,
    key_id: LabelValue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_graph_errors() {
        let body = r#"{"error":{"code":"Authorization_RequestDenied","message":"Insufficient privileges to complete the operation.","innerError":{"date":"2026-09-28T10:00:00"}}}"#;
        assert_eq!(
            describe_graph_error(StatusCode::FORBIDDEN, body),
            "Graph returned 403 Forbidden: Authorization_RequestDenied: \
             Insufficient privileges to complete the operation."
        );
    }

    #[test]
    fn describes_non_graph_errors_verbatim() {
        let body = "<html>Bad Gateway</html>";
        assert_eq!(
            describe_graph_error(StatusCode::BAD_GATEWAY, body),
            "Graph returned 502 Bad Gateway: <html>Bad Gateway</html>"
        );

        let long = "x".repeat(GRAPH_ERROR_BODY_MAX_CHARS * 2);
        let described = describe_graph_error(StatusCode::BAD_GATEWAY, &long);
        assert!(described.contains(&"x".repeat(GRAPH_ERROR_BODY_MAX_CHARS)));
        assert!(!described.contains(&"x".repeat(GRAPH_ERROR_BODY_MAX_CHARS + 1)));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use oauth2::basic::BasicTokenType;
    use oauth2::{AccessToken, ClientId, ClientSecret, EmptyExtraTokenFields};

    /// A Graph client that already holds a valid token and sends its requests
    /// to `applications_endpoint` instead of Microsoft Graph.
    pub(crate) async fn graph_client(applications_endpoint: String) -> AzureGraphClient {
        let settings = AppSettings {
            azure_client_id: ClientId::new("client-id".to_string()),
            azure_client_secret: ClientSecret::new("client-secret".to_string()),
            azure_tenant_id: "tenant-id".to_string(),
            port: 0,
        };
        let token_provider = AzureClientTokenProvider::init(&settings).unwrap();
        *token_provider.token.write().await = Some(Token {
            token_response: BasicTokenResponse::new(
                AccessToken::new("test-token".to_string()),
                BasicTokenType::Bearer,
                EmptyExtraTokenFields {},
            ),
            expires_at: Instant::now() + Duration::from_secs(3600),
        });
        AzureGraphClient::new(Arc::new(token_provider), applications_endpoint, false).unwrap()
    }

    /// Build a scrape registry with fixed data, mirroring what `scrape` produces.
    pub(crate) fn scrape_registry() -> Registry {
        let mut registry = <Registry>::default();
        let credentials_metric = Family::<CredentialLabels, Gauge<u64, AtomicU64>>::default();
        registry.register_with_unit(
            "credential_expiration_time",
            "Timestamp of credential expiration",
            Unit::Seconds,
            credentials_metric.clone(),
        );

        for (app_name, app_id, key_id, ts) in [
            ("First App", "aaaa-1111", "key-1", 1_700_000_000_u64),
            ("Second \"quoted\" App", "bbbb-2222", "key-2", 1_800_000_000),
            ("Backslash \\ App", "cccc-3333", "key-3", 1_900_000_000),
        ] {
            credentials_metric
                .get_or_create(&CredentialLabels {
                    app_name: app_name.into(),
                    app_id: app_id.into(),
                    key_id: key_id.into(),
                })
                .set(ts);
        }
        registry
    }
}
