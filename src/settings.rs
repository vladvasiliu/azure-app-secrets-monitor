use anyhow::Result;
use config::{Config, Environment, File};
use oauth2::{ClientId, ClientSecret};

pub struct AppSettings {
    pub azure_client_id: ClientId,
    pub azure_client_secret: ClientSecret,
    pub azure_tenant_id: String,
}

impl AppSettings {
    pub fn fetch() -> Result<Self> {
        let config = Config::builder()
            .add_source(File::with_name("config"))
            .add_source(Environment::with_prefix("AASM"))
            .build()?;

        Ok(Self {
            azure_client_id: config.get::<ClientId>("azure_client_id")?,
            azure_client_secret: config.get::<ClientSecret>("azure_client_secret")?,
            azure_tenant_id: config.get_string("azure_tenant_id")?,
        })
    }
}
