use anyhow::{Result, bail};
use config::{Config, ConfigError, Environment, File};
use oauth2::{ClientId, ClientSecret};
use serde::de::DeserializeOwned;

static DEFAULT_PORT: u16 = 9912;
static ENV_PREFIX: &str = "AASM";

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
            .add_source(File::with_name("config").required(false))
            .add_source(Environment::with_prefix(ENV_PREFIX))
            .build()?;

        Self::from_config(&config)
    }

    fn from_config(config: &Config) -> Result<Self> {
        let mut reader = FieldReader {
            config,
            problems: Vec::new(),
        };

        let azure_client_id = reader.get::<ClientId>("azure_client_id");
        let azure_client_secret = reader.get::<ClientSecret>("azure_client_secret");
        let azure_tenant_id = reader.get::<String>("azure_tenant_id");
        let port = reader.get::<i64>("port").and_then(|port| {
            let converted = u16::try_from(port).ok();
            if converted.is_none() {
                reader.problems.push(format!("port out of range: {port}"));
            }
            converted
        });

        match (azure_client_id, azure_client_secret, azure_tenant_id, port) {
            (
                Some(azure_client_id),
                Some(azure_client_secret),
                Some(azure_tenant_id),
                Some(port),
            ) => Ok(Self {
                azure_client_id,
                azure_client_secret,
                azure_tenant_id,
                port,
            }),
            _ => bail!("Invalid configuration: {}", reader.problems.join("; ")),
        }
    }
}

/// Reads configuration fields while collecting every problem, so that they are
/// all reported at once rather than one per start.
struct FieldReader<'a> {
    config: &'a Config,
    problems: Vec<String>,
}

impl FieldReader<'_> {
    fn get<T: DeserializeOwned>(&mut self, key: &str) -> Option<T> {
        match self.config.get(key) {
            Ok(value) => Some(value),
            Err(ConfigError::NotFound(_)) => {
                self.problems.push(format!(
                    "missing {key} (set {ENV_PREFIX}_{})",
                    key.to_uppercase()
                ));
                None
            }
            Err(err) => {
                self.problems.push(err.to_string());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_error(builder: config::ConfigBuilder<config::builder::DefaultState>) -> String {
        let config = builder.build().unwrap();
        match AppSettings::from_config(&config) {
            Ok(_) => panic!("configuration unexpectedly valid"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn reports_every_missing_field() {
        assert_eq!(
            settings_error(Config::builder()),
            "Invalid configuration: \
             missing azure_client_id (set AASM_AZURE_CLIENT_ID); \
             missing azure_client_secret (set AASM_AZURE_CLIENT_SECRET); \
             missing azure_tenant_id (set AASM_AZURE_TENANT_ID); \
             missing port (set AASM_PORT)"
        );
    }

    #[test]
    fn reports_invalid_values_with_missing_fields() {
        let builder = Config::builder()
            .set_override("azure_client_id", "id")
            .unwrap()
            .set_override("port", 70_000)
            .unwrap();
        assert_eq!(
            settings_error(builder),
            "Invalid configuration: \
             missing azure_client_secret (set AASM_AZURE_CLIENT_SECRET); \
             missing azure_tenant_id (set AASM_AZURE_TENANT_ID); \
             port out of range: 70000"
        );
    }

    #[test]
    fn accepts_complete_configuration() {
        let config = Config::builder()
            .set_override("azure_client_id", "id")
            .unwrap()
            .set_override("azure_client_secret", "secret")
            .unwrap()
            .set_override("azure_tenant_id", "tenant")
            .unwrap()
            .set_override("port", 9912)
            .unwrap()
            .build()
            .unwrap();
        let settings = AppSettings::from_config(&config).unwrap();
        assert_eq!(settings.azure_client_id.as_str(), "id");
        assert_eq!(settings.azure_tenant_id, "tenant");
        assert_eq!(settings.port, 9912);
    }
}
