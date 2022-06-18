use crate::AppSettings;
use anyhow::Result;
use chrono::{DateTime, Utc};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, ClientId, ClientSecret, Scope, TokenResponse, TokenUrl};
use reqwest::Client;
use serde::Deserialize;
use std::fmt::{Display, Formatter};
use std::time::Duration;

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

static AZURE_BASE_URL: &str = "https://login.microsoftonline.com";
static AZURE_AUTH_PATH: &str = "oauth2/v2.0/authorize";
static AZURE_TOKEN_PATH: &str = "oauth2/v2.0/token";
static AZURE_SCOPE: &str = "https://graph.microsoft.com/.default";
static AZURE_APPLICATIONS_ENDPOINT: &str = "https://graph.microsoft.com/v1.0/applications/";

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

pub struct AzureClient {
    client_id: ClientId,
    client_secret: ClientSecret,
    auth_url: AuthUrl,
    token_url: TokenUrl,
}

impl AzureClient {
    pub fn from_settings(settings: &AppSettings) -> Result<Self> {
        let auth_url = AuthUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, &settings.azure_tenant_id, AZURE_AUTH_PATH
        ))?;
        let token_url = TokenUrl::new(format!(
            "{}/{}/{}",
            AZURE_BASE_URL, &settings.azure_tenant_id, AZURE_TOKEN_PATH
        ))?;
        Ok(Self {
            client_id: settings.azure_client_id.to_owned(),
            client_secret: settings.azure_client_secret.to_owned(),
            auth_url,
            token_url,
        })
    }

    pub async fn work(&self) -> Result<()> {
        let oauth_client = BasicClient::new(
            self.client_id.to_owned(),
            Some(self.client_secret.to_owned()),
            self.auth_url.to_owned(),
            Some(self.token_url.to_owned()),
        );

        let token_result = oauth_client
            .exchange_client_credentials()
            .add_scope(Scope::new(AZURE_SCOPE.to_string()))
            .request_async(async_http_client)
            .await?;

        let client = Client::builder()
            .user_agent(APP_USER_AGENT)
            .gzip(true)
            .brotli(true)
            .timeout(Duration::from_secs(2))
            .https_only(true)
            .build()?;

        let query = client
            .get(AZURE_APPLICATIONS_ENDPOINT)
            .query(&[(
                "$select",
                "appId,displayName,keyCredentials,passwordCredentials",
            )])
            .bearer_auth(token_result.access_token().secret())
            .build()?;

        let response = client.execute(query).await?;

        let body = response.json::<ResponsePage>().await?;
        for app in body.value {
            println!("{}", app);
        }

        Ok(())
    }
}
