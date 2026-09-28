mod azure;
mod exporter;
mod logging;
mod settings;

use crate::azure::{AzureClientTokenProvider, AzureGraphClient};
use crate::exporter::Exporter;
use crate::settings::AppSettings;
use anyhow::Result;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tracing::error;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    logging::init();

    // Reported through the logger rather than returned, so that startup errors
    // are formatted like every other log line.
    if let Err(err) = run().await {
        error!(error = %format!("{err:#}"), "Startup failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let settings = AppSettings::fetch()?;

    let token_provider = Arc::new(AzureClientTokenProvider::init(&settings)?);
    let azure_client = AzureGraphClient::with_token_provider(token_provider.clone())?;

    tokio::task::spawn(async move {
        token_provider.work_cache().await;
    });

    let listen: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), settings.port);
    let exporter = Exporter::new(listen, azure_client);

    exporter.run().await;

    Ok(())
}
