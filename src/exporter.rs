use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use prometheus_client::encoding::text::{encode, Encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;
use std::io::{Error, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tracing::{error, info, warn};

#[async_trait]
pub trait PromScraper {
    async fn scrape(&self) -> Result<Registry>;

    /// Return whether the scraper is ready to go.
    /// The contained message will be displayed on the `/status` page.
    async fn ready(&self) -> std::result::Result<String, String>;

    fn name(&self) -> &str;
}

#[derive(Clone, Eq, Hash, PartialEq, Encode)]
pub struct SuccessMetricLabels {
    outcome: Outcome,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

impl Encode for Outcome {
    fn encode(&self, writer: &mut dyn Write) -> std::result::Result<(), Error> {
        let str = match self {
            Self::Failure => "failure",
            Self::Success => "success",
        };
        write!(writer, "{}", str)
    }
}

pub struct Exporter<T: PromScraper> {
    socket: SocketAddr,
    home_page: Html<String>,
    scraper: Arc<T>,
}

impl<T: PromScraper + Send + Sync + 'static> Exporter<T> {
    pub fn new(socket: SocketAddr, scraper: T) -> Self {
        let home_page: Html<String> = Html::from(format!(
            "<html>\
                <head><title>{name} Exporter</title>\
                <body>\
                    <h1>{name} Exporter</h1>
                    <br />
                    <p><a href=\"/status\">Exporter status</a></p>
                    <p><a href=\"/metrics\">Metrics</a></p>
                </body>\
            </html>",
            name = scraper.name()
        ));
        Self::with_home_page(socket, scraper, home_page)
    }

    pub fn with_home_page(socket: SocketAddr, scraper: T, home_page: Html<String>) -> Self {
        Self {
            socket,
            scraper: Arc::new(scraper),
            home_page,
        }
    }

    pub async fn run(&self) {
        let mut registry = <Registry>::default();
        let success_metric = Family::<SuccessMetricLabels, Counter>::default();
        registry.register(
            "scrape_status",
            "Whether the scrape was successful",
            Box::new(success_metric.clone()),
        );
        let success_metric = Arc::new(success_metric);
        let info_metric = Info::new(vec![("version", env!["CARGO_PKG_VERSION"])]);
        registry.register(
            "azure_app_secrets_monitor_build_info",
            "Information about the scraper itself",
            Box::new(info_metric),
        );
        let registry = Arc::new(registry);
        let home_page = self.home_page.clone();
        let app = Router::new()
            .route("/", get(|| async { home_page }))
            .route(
                "/status",
                get({
                    let scraper = Arc::clone(&self.scraper);
                    move || status(scraper)
                }),
            )
            .route(
                "/metrics",
                get({
                    let scraper = Arc::clone(&self.scraper);
                    let success_metric = Arc::clone(&success_metric);
                    let registry = Arc::clone(&registry);
                    || async move { get_metrics(&*scraper, &success_metric, &registry).await }
                }),
            );
        let server = axum::Server::bind(&self.socket).serve(app.into_make_service());
        info!("Listening on {}", server.local_addr());
        let graceful = server.with_graceful_shutdown(shutdown_signal());
        match graceful.await.map_err(axum::Error::new) {
            Ok(()) => info!("Exporter is shut down"),
            Err(err) => error!("Server error: {}", err),
        }
    }
}

async fn status<T: PromScraper + Send + Sync + 'static>(scraper: Arc<T>) -> impl IntoResponse {
    match scraper.ready().await {
        Ok(msg) => msg.into_response(),
        Err(err) => (StatusCode::SERVICE_UNAVAILABLE, err).into_response(),
    }
}

async fn get_metrics<S: PromScraper + Send + Sync + 'static>(
    scraper: &S,
    success_metric: &Family<SuccessMetricLabels, Counter>,
    registry: &Registry,
) -> Response {
    let mut registries = vec![registry];
    let scrape_result = scraper.scrape().await;
    let scrape_registry;
    let outcome = match scrape_result {
        Ok(scrape_reg) => {
            scrape_registry = scrape_reg;
            registries.push(&scrape_registry);
            Outcome::Success
        }
        Err(err) => {
            warn!("Scrape failed: {}", err);
            Outcome::Failure
        }
    };
    success_metric
        .get_or_create(&SuccessMetricLabels { outcome })
        .inc();
    match output_metrics(registries) {
        Ok(output) => output,
        Err(err) => {
            let msg = format!("Metrics output failed: {}", err);
            warn!(msg);
            (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
        }
    }
}

fn output_metrics(registries: Vec<&Registry>) -> Result<Response> {
    let mut buffer = vec![];
    encode(&mut buffer, &registries).context("Registry encoding failed")?;
    let result =
        String::from_utf8(buffer).context("Failed to parse UTF-8 from encoded registry")?;
    let response = (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        result,
    )
        .into_response();
    Ok(response)
}

// Lifted from https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("signal received, starting graceful shutdown");
}
