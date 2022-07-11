use crate::AppSettings;
use anyhow::{anyhow, Context, Result};
use chrono::{Date, DateTime, Utc};
use oauth2::basic::{BasicClient as Oauth2BasicClient, BasicTokenResponse};
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, Scope, TokenResponse, TokenUrl};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

static AZURE_BASE_URL: &str = "https://login.microsoftonline.com";
static AZURE_AUTH_PATH: &str = "oauth2/v2.0/authorize";
static AZURE_TOKEN_PATH: &str = "oauth2/v2.0/token";
static AZURE_SCOPE: &str = "https://graph.microsoft.com/.default";
static AZURE_APPLICATIONS_ENDPOINT: &str = "https://graph.microsoft.com/v1.0/applications/";
static AZURE_TOKEN_MIN_LIFETIME: u64 = 60;
static AZURE_TOKEN_FETCH_RETRY: u64 = 10;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Credentials {
    custom_key_identifier: Option<String>,
    display_name: Option<String>,
    end_date_time: DateTime<Utc>,
    hint: Option<String>,
    key_id: String,
    start_date_time: DateTime<Utc>,
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

pub struct AzureClientTokenProvider {
    oauth2_client: Oauth2BasicClient,
    token: RwLock<Option<Token>>,
}

impl AzureClientTokenProvider {
    pub fn init(settings: &AppSettings) -> Result<Self> {
        let auth_url = AuthUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, &settings.azure_tenant_id, AZURE_AUTH_PATH
        ))?;
        let token_url = TokenUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, &settings.azure_tenant_id, AZURE_TOKEN_PATH
        ))?;
        let oauth2_client = Oauth2BasicClient::new(
            settings.azure_client_id.to_owned(),
            Some(settings.azure_client_secret.to_owned()),
            auth_url,
            Some(token_url),
        );

        Ok(Self {
            oauth2_client,
            token: RwLock::new(None),
        })
    }

    async fn refresh(&self) -> Result<Instant> {
        let result = self
            .oauth2_client
            .exchange_client_credentials()
            .add_scope(Scope::new(AZURE_SCOPE.to_string()))
            .request_async(async_http_client)
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
            let deadline = match self.refresh().await {
                Ok(instant) => instant,
                Err(err) => {
                    // TODO warn!("Failed to refresh Azure token: {}", err);
                    Instant::now() + Duration::from_secs(AZURE_TOKEN_FETCH_RETRY)
                }
            };

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
}

impl AzureGraphClient {
    pub fn with_token_provider(token_provider: Arc<AzureClientTokenProvider>) -> Result<Self> {
        let http_client = HttpClient::builder()
            .user_agent(APP_USER_AGENT)
            .gzip(true)
            .brotli(true)
            .timeout(Duration::from_secs(2))
            .https_only(true)
            .build()?;

        Ok(Self {
            http_client,
            token_provider,
        })
    }

    pub async fn work(&self) -> Result<()> {
        let query = self
            .http_client
            .get(AZURE_APPLICATIONS_ENDPOINT)
            .query(&[(
                "$select",
                "appId,displayName,keyCredentials,passwordCredentials",
            )])
            .bearer_auth(self.token_provider.get_secret().await?)
            .build()?;

        let response = self.http_client.execute(query).await?;

        let body = response.json::<ResponsePage>().await?;
        for app in body.value {
            println!("{}", app);
        }

        Ok(())
    }
}
