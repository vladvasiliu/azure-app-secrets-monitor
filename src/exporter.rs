use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use prometheus_client::encoding::text::{encode, Encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use std::io::{Error, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

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
    // registry: Arc<Registry>,
    // success_metric: Arc<Family<SuccessMetricLabels, Counter>>,
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
            // success_metric: Arc::new(success_metric),
            home_page,
        }
    }

    pub async fn run(&self) -> Result<(), axum::Error> {
        let mut registry = <Registry>::default();
        let success_metric = Family::<SuccessMetricLabels, Counter>::default();
        registry.register(
            "scrape_status",
            "Whether the scrape was successful",
            Box::new(success_metric.clone()),
        );
        let success_metric = Arc::new(success_metric);
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
                    || async move { get_metrics(&*scraper, &*success_metric, &*registry).await }
                }),
            );
        axum::Server::bind(&self.socket)
            .serve(app.into_make_service())
            .await
            .map_err(axum::Error::new)
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
    Ok(result.into_response())
}
