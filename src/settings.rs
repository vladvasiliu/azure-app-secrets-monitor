use anyhow::{anyhow, Result};
use config::{Config, Environment, File};
use oauth2::{ClientId, ClientSecret};

static DEFAULT_PORT: u16 = 9912;

pub struct AppSettings {
    pub azure_client_id: ClientId,
    pub azure_client_secret: ClientSecret,
    pub azure_tenant_id: String,
    pub port: u16,
}

impl AppSettings {
    pub fn fetch() -> Result<Self> {
        let config = Config::builder()
            .set_default("port", DEFAULT_PORT)?
            .add_source(File::with_name("config"))
            .add_source(Environment::with_prefix("AASM"))
            .build()?;

        let config_port = config.get_int("port")?;
        let port = config_port
            .try_into()
            .map_err(|_| anyhow!("Port out of range: {}", config_port))?;

        Ok(Self {
            azure_client_id: config.get::<ClientId>("azure_client_id")?,
            azure_client_secret: config.get::<ClientSecret>("azure_client_secret")?,
            azure_tenant_id: config.get_string("azure_tenant_id")?,
            port,
        })
    }
}
