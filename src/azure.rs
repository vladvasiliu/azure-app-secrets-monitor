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
use reqwest::header::RETRY_AFTER;

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
/// Timeout of a single Graph request, body included.
static AZURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Total time a scrape may spend on Graph, all pages and retries included.
/// Keep it below Prometheus' `scrape_timeout` for this target.
static AZURE_SCRAPE_BUDGET: Duration = Duration::from_secs(30);
/// Attempts per Graph page, the first one included.
static AZURE_MAX_ATTEMPTS: u32 = 3;
static AZURE_BACKOFF_BASE: Duration = Duration::from_millis(500);
static AZURE_BACKOFF_MAX: Duration = Duration::from_secs(5);
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

/// How Graph requests are retried within one scrape.
#[derive(Clone, Debug)]
struct RetryPolicy {
    scrape_budget: Duration,
    attempt_timeout: Duration,
    max_attempts: u32,
    backoff_base: Duration,
    backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            scrape_budget: AZURE_SCRAPE_BUDGET,
            attempt_timeout: AZURE_TIMEOUT,
            max_attempts: AZURE_MAX_ATTEMPTS,
            backoff_base: AZURE_BACKOFF_BASE,
            backoff_max: AZURE_BACKOFF_MAX,
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff with full jitter: a random delay up to
    /// `backoff_base * 2^(attempt - 1)`, capped by `backoff_max`.
    fn backoff(&self, attempt: u32) -> Duration {
        let ceiling = self
            .backoff_base
            .saturating_mul(1 << attempt.saturating_sub(1).min(16))
            .min(self.backoff_max);
        ceiling.mul_f64(rand::random::<f64>())
    }
}

/// Why one attempt at fetching a Graph page failed.
enum AttemptError {
    /// Worth trying again, after `retry_after` if Graph asked for a delay.
    Transient {
        error: anyhow::Error,
        retry_after: Option<Duration>,
    },
    /// Retrying would not help.
    Permanent(anyhow::Error),
}

/// Statuses Graph returns when throttling or briefly unavailable.
fn is_transient_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// The delay Graph asks for in `Retry-After`. Only the delay-seconds form is
/// understood, which is the one Graph sends.
fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

pub struct AzureGraphClient {
    token_provider: Arc<AzureClientTokenProvider>,
    http_client: HttpClient,
    applications_endpoint: String,
    retry_policy: RetryPolicy,
}

impl AzureGraphClient {
    pub fn with_token_provider(token_provider: Arc<AzureClientTokenProvider>) -> Result<Self> {
        Self::new(
            token_provider,
            AZURE_APPLICATIONS_ENDPOINT.to_string(),
            true,
            RetryPolicy::default(),
        )
    }

    /// `https_only` is only turned off by tests, which talk to a local mock.
    fn new(
        token_provider: Arc<AzureClientTokenProvider>,
        applications_endpoint: String,
        https_only: bool,
        retry_policy: RetryPolicy,
    ) -> Result<Self> {
        // No client-wide timeout: each request gets its own, see `fetch_page`.
        let http_client = HttpClient::builder()
            .user_agent(APP_USER_AGENT)
            .gzip(true)
            .https_only(https_only)
            .build()?;

        Ok(Self {
            http_client,
            token_provider,
            applications_endpoint,
            retry_policy,
        })
    }

    /// Fetch one page of applications, retrying transient failures until
    /// `deadline`. Retrying per page means a failure on a later page does not
    /// refetch the earlier ones.
    async fn fetch_page(
        &self,
        url: &str,
        query: Option<&[(&str, &str)]>,
        page: u32,
        deadline: Instant,
    ) -> Result<ResponsePage> {
        let policy = &self.retry_policy;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "Scrape budget of {}s used up before Graph page {page}",
                    policy.scrape_budget.as_secs()
                ));
            }
            let timeout = remaining.min(policy.attempt_timeout);
            let (error, retry_after) = match self.try_fetch_page(url, query, page, timeout).await {
                Ok(body) => return Ok(body),
                Err(AttemptError::Permanent(error)) => return Err(error),
                Err(AttemptError::Transient { error, retry_after }) => (error, retry_after),
            };

            let delay = retry_after.unwrap_or_else(|| policy.backoff(attempt));
            if attempt >= policy.max_attempts || Instant::now() + delay >= deadline {
                return Err(error.context(format!("Gave up after {attempt} attempts")));
            }
            let http_error = error
                .chain()
                .find_map(|e| e.downcast_ref::<UpstreamHttpError>());
            warn!(
                action = "graph-request",
                outcome = "failure",
                page,
                attempt,
                retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                error = %format!("{error:#}"),
                http_status = http_error.map(|e| e.status),
                request_id = http_error.and_then(|e| e.request_id.as_deref()),
                "Graph request failed, retrying"
            );
            tokio::time::sleep(delay).await;
        }
    }

    async fn try_fetch_page(
        &self,
        url: &str,
        query: Option<&[(&str, &str)]>,
        page: u32,
        timeout: Duration,
    ) -> std::result::Result<ResponsePage, AttemptError> {
        let token = self
            .token_provider
            .get_secret()
            .await
            .map_err(AttemptError::Permanent)?;
        let mut request = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .timeout(timeout);
        if let Some(query) = query {
            request = request.query(query);
        }

        let transport_error = |err: reqwest::Error| {
            let retryable = !err.is_builder();
            let error =
                anyhow::Error::from(err).context(format!("Graph request for page {page} failed"));
            if retryable {
                AttemptError::Transient {
                    error,
                    retry_after: None,
                }
            } else {
                AttemptError::Permanent(error)
            }
        };

        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(&response);
            let error = graph_error(response).await;
            return Err(if is_transient_status(status) {
                AttemptError::Transient { error, retry_after }
            } else {
                AttemptError::Permanent(error)
            });
        }

        // Read the body before decoding it, so that a timeout or dropped
        // connection while reading is retried, but malformed JSON is not.
        let body = response.bytes().await.map_err(transport_error)?;
        serde_json::from_slice(&body)
            .with_context(|| format!("Failed to decode Graph response page {page}"))
            .map_err(AttemptError::Permanent)
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

        let deadline = Instant::now() + self.retry_policy.scrape_budget;
        let mut url = self.applications_endpoint.clone();
        // `@odata.nextLink` already carries the query, so only the first page
        // needs one.
        let mut query: Option<&[(&str, &str)]> = Some(&[(
            "$select",
            "appId,displayName,keyCredentials,passwordCredentials",
        )]);

        let mut page: u32 = 0;
        loop {
            page += 1;
            let body = self.fetch_page(&url, query, page, deadline).await?;
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

            match body.next_link {
                Some(next_link) => {
                    url = next_link;
                    query = None;
                }
                None => break,
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

    mod retries {
        use super::super::test_support::graph_client_with_retries;
        use super::*;
        use axum::Router;
        use axum::extract::RawQuery;
        use axum::response::{IntoResponse, Response as AxumResponse};
        use axum::routing::get;
        use serde_json::{Value, json};
        use std::collections::VecDeque;
        use std::sync::Mutex;
        use tokio::net::TcpListener;

        /// What the mock Graph answers to one request.
        enum Reply {
            Status(StatusCode, Option<&'static str>),
            Page(Value),
            Slow(Duration, Value),
        }

        struct MockGraph {
            endpoint: String,
            queries: Arc<Mutex<Vec<String>>>,
        }

        impl MockGraph {
            fn queries(&self) -> Vec<String> {
                self.queries.lock().unwrap().clone()
            }
        }

        /// Serve the replies built by `replies`, one per request, in order.
        /// `replies` gets the applications URL, to build next links with.
        async fn serve_graph(replies: impl FnOnce(&str) -> Vec<Reply>) -> MockGraph {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/applications/", listener.local_addr().unwrap());
            let replies = Arc::new(Mutex::new(VecDeque::from(replies(&endpoint))));
            let queries = Arc::new(Mutex::new(Vec::new()));
            let handler = {
                let queries = Arc::clone(&queries);
                move |RawQuery(query): RawQuery| async move {
                    queries.lock().unwrap().push(query.unwrap_or_default());
                    let reply = replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected request");
                    let response: AxumResponse = match reply {
                        Reply::Status(status, None) => status.into_response(),
                        Reply::Status(status, Some(retry_after)) => {
                            (status, [("retry-after", retry_after)]).into_response()
                        }
                        Reply::Page(body) => {
                            ([("content-type", "application/json")], body.to_string())
                                .into_response()
                        }
                        Reply::Slow(delay, body) => {
                            tokio::time::sleep(delay).await;
                            ([("content-type", "application/json")], body.to_string())
                                .into_response()
                        }
                    };
                    response
                }
            };
            let app = Router::new().route("/applications/", get(handler));
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            MockGraph { endpoint, queries }
        }

        fn page(app_id: &str, next_link: Option<String>) -> Value {
            let mut body = json!({
                "value": [{
                    "appId": app_id,
                    "displayName": format!("App {app_id}"),
                    "passwordCredentials": [{"keyId": format!("key-{app_id}"), "endDateTime": "2030-01-01T00:00:00Z"}],
                    "keyCredentials": [],
                }]
            });
            if let Some(next_link) = next_link {
                body["@odata.nextLink"] = next_link.into();
            }
            body
        }

        fn fast_policy() -> RetryPolicy {
            RetryPolicy {
                scrape_budget: Duration::from_secs(5),
                attempt_timeout: Duration::from_millis(200),
                max_attempts: 3,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
            }
        }

        async fn scrape(graph: &MockGraph) -> Result<Registry> {
            graph_client_with_retries(graph.endpoint.clone(), fast_policy())
                .await
                .scrape()
                .await
        }

        fn app_ids(registry: &Registry) -> Vec<String> {
            let mut output = String::new();
            prometheus_client::encoding::text::encode(&mut output, registry).unwrap();
            output
                .lines()
                .filter_map(|line| line.split("app_id=\"").nth(1))
                .map(|rest| rest.split('"').next().unwrap().to_string())
                .collect()
        }

        #[tokio::test]
        async fn retries_transient_statuses() {
            let graph = serve_graph(|_| {
                vec![
                    Reply::Status(StatusCode::SERVICE_UNAVAILABLE, None),
                    Reply::Status(StatusCode::TOO_MANY_REQUESTS, Some("0")),
                    Reply::Page(page("a", None)),
                ]
            })
            .await;

            let registry = scrape(&graph).await.unwrap();
            assert_eq!(app_ids(&registry), ["a"]);
            assert_eq!(graph.queries().len(), 3);
        }

        #[tokio::test]
        async fn retries_timeouts() {
            let graph = serve_graph(|_| {
                vec![
                    Reply::Slow(Duration::from_secs(2), page("slow", None)),
                    Reply::Page(page("a", None)),
                ]
            })
            .await;

            let registry = scrape(&graph).await.unwrap();
            assert_eq!(app_ids(&registry), ["a"]);
        }

        #[tokio::test]
        async fn does_not_retry_client_errors() {
            let graph = serve_graph(|_| vec![Reply::Status(StatusCode::FORBIDDEN, None)]).await;

            let err = scrape(&graph).await.unwrap_err();
            assert_eq!(graph.queries().len(), 1);
            assert!(!format!("{err:#}").contains("Gave up"), "{err:#}");
        }

        #[tokio::test]
        async fn gives_up_after_max_attempts() {
            let graph = serve_graph(|_| {
                (0..3)
                    .map(|_| Reply::Status(StatusCode::BAD_GATEWAY, None))
                    .collect()
            })
            .await;

            let err = scrape(&graph).await.unwrap_err();
            assert_eq!(graph.queries().len(), 3);
            assert!(
                format!("{err:#}").starts_with("Gave up after 3 attempts: Graph returned 502"),
                "{err:#}"
            );
            let http_error = err
                .chain()
                .find_map(|e| e.downcast_ref::<UpstreamHttpError>())
                .unwrap();
            assert_eq!(http_error.status, 502);
        }

        #[tokio::test]
        async fn gives_up_when_retry_after_exceeds_budget() {
            let graph =
                serve_graph(|_| vec![Reply::Status(StatusCode::TOO_MANY_REQUESTS, Some("3600"))])
                    .await;

            let err = scrape(&graph).await.unwrap_err();
            assert_eq!(graph.queries().len(), 1);
            assert!(
                format!("{err:#}").starts_with("Gave up after 1 attempts"),
                "{err:#}"
            );
        }

        /// A failing page is retried on its own: earlier pages are not refetched,
        /// and the next link is followed without adding a query of its own.
        #[tokio::test]
        async fn retries_pages_individually() {
            let graph = serve_graph(|endpoint| {
                let next = format!("{endpoint}?$skiptoken=2");
                vec![
                    Reply::Page(page("a", Some(next))),
                    Reply::Status(StatusCode::GATEWAY_TIMEOUT, None),
                    Reply::Page(page("b", None)),
                ]
            })
            .await;

            let registry = scrape(&graph).await.unwrap();
            let mut ids = app_ids(&registry);
            ids.sort();
            assert_eq!(ids, ["a", "b"]);
            let queries = graph.queries();
            assert_eq!(queries.len(), 3);
            assert!(queries[0].starts_with("%24select="), "{queries:?}");
            assert_eq!(queries[1..], ["$skiptoken=2", "$skiptoken=2"]);
        }
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
        graph_client_with_retries(applications_endpoint, RetryPolicy::default()).await
    }

    pub(super) async fn graph_client_with_retries(
        applications_endpoint: String,
        retry_policy: RetryPolicy,
    ) -> AzureGraphClient {
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
        AzureGraphClient::new(
            Arc::new(token_provider),
            applications_endpoint,
            false,
            retry_policy,
        )
        .unwrap()
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
