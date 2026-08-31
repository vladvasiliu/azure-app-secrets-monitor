use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use prometheus_client::encoding::text::{encode_eof, encode_registry};
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
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

#[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelSet)]
pub struct SuccessMetricLabels {
    outcome: Outcome,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

impl EncodeLabelValue for Outcome {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> std::result::Result<(), std::fmt::Error> {
        let str = match self {
            Self::Failure => "failure",
            Self::Success => "success",
        };
        write!(encoder, "{}", str)
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
            success_metric.clone(),
        );
        let success_metric = Arc::new(success_metric);
        let info_metric = Info::new(vec![("version", env!["CARGO_PKG_VERSION"])]);
        registry.register(
            "azure_app_secrets_monitor_build",
            "Information about the scraper itself",
            info_metric,
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
        let listener = match TcpListener::bind(&self.socket).await {
            Ok(listener) => listener,
            Err(err) => {
                error!("Failed to bind to {}: {}", self.socket, err);
                return;
            }
        };
        match listener.local_addr() {
            Ok(addr) => info!("Listening on {}", addr),
            Err(err) => warn!("Failed to get local address: {}", err),
        }
        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
        match server.await {
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
    output_metrics(registries).unwrap_or_else(|err| {
        let msg = format!("Metrics output failed: {}", err);
        warn!(msg);
        (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
    })
}

fn encode_registries(registries: Vec<&Registry>) -> Result<String> {
    let mut buffer = String::new();
    for registry in registries {
        encode_registry(&mut buffer, registry).context("Registry encoding failed")?;
    }
    encode_eof(&mut buffer).context("Registry encoding failed")?;
    Ok(buffer)
}

fn output_metrics(registries: Vec<&Registry>) -> Result<Response> {
    let result = encode_registries(registries)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::test_support::scrape_registry;

    /// Pins the OpenMetrics payload served by `/metrics`, including the way the
    /// exporter's own registry is concatenated with the per-scrape one.
    #[test]
    fn encodes_multiple_registries() {
        let mut registry = <Registry>::default();
        let success_metric = Family::<SuccessMetricLabels, Counter>::default();
        registry.register(
            "scrape_status",
            "Whether the scrape was successful",
            success_metric.clone(),
        );
        registry.register(
            "azure_app_secrets_monitor_build",
            "Information about the scraper itself",
            Info::new(vec![("version", "9.9.9")]),
        );

        success_metric
            .get_or_create(&SuccessMetricLabels {
                outcome: Outcome::Success,
            })
            .inc();
        for _ in 0..3 {
            success_metric
                .get_or_create(&SuccessMetricLabels {
                    outcome: Outcome::Failure,
                })
                .inc();
        }

        let scrape_reg = scrape_registry();
        let output = encode_registries(vec![&registry, &scrape_reg]).unwrap();

        // A `Family`'s series come out in `HashMap` order, so compare as a set.
        let mut got: Vec<&str> = output.lines().collect();
        got.sort_unstable();
        let mut want = vec![
            "# HELP scrape_status Whether the scrape was successful.",
            "# TYPE scrape_status counter",
            "scrape_status_total{outcome=\"failure\"} 3",
            "scrape_status_total{outcome=\"success\"} 1",
            "# HELP azure_app_secrets_monitor_build Information about the scraper itself.",
            "# TYPE azure_app_secrets_monitor_build info",
            "azure_app_secrets_monitor_build_info{version=\"9.9.9\"} 1",
            "# HELP credential_expiration_time_seconds Timestamp of credential expiration.",
            "# TYPE credential_expiration_time_seconds gauge",
            "# UNIT credential_expiration_time_seconds seconds",
            "credential_expiration_time_seconds{app_id=\"aaaa-1111\",app_name=\"First App\",key_id=\"key-1\"} 1700000000",
            "credential_expiration_time_seconds{app_id=\"bbbb-2222\",app_name=\"Second \"quoted\" App\",key_id=\"key-2\"} 1800000000",
            "credential_expiration_time_seconds{app_id=\"cccc-3333\",app_name=\"Backslash \\ App\",key_id=\"key-3\"} 1900000000",
            "# EOF",
        ];
        want.sort_unstable();
        assert_eq!(got, want);

        // The terminator belongs at the very end, exactly once.
        assert!(output.ends_with("# EOF\n"));
        assert_eq!(output.matches("# EOF").count(), 1);
    }
}
