use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Router, Server};
use oauth2::http::header;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::Arc;

#[async_trait]
pub trait PromScraper {
    type ScrapeError: Display;

    async fn scrape(&self) -> Result<Registry, Self::ScrapeError>;

    /// Return wether the scraper is ready to go.
    /// The contained message will be displayed on the `/status` page.
    async fn ready(&self) -> Result<String, String>;

    fn name(&self) -> &str;
}

pub struct Exporter<T: PromScraper> {
    socket: SocketAddr,
    home_page: Html<String>,
    registry: Registry,
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
        let registry = <Registry>::default();

        Self {
            socket,
            registry,
            scraper: Arc::new(scraper),
            home_page,
        }
    }

    pub async fn run(&self) -> Result<(), axum::Error> {
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
                    move || metrics(scraper)
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

async fn metrics<T: PromScraper + Send + Sync + 'static>(scraper: Arc<T>) -> impl IntoResponse {
    match scraper.scrape().await {
        Ok(registry) => {
            let mut buffer = vec![];
            encode(&mut buffer, &registry).unwrap();
            (
                [(
                    header::CONTENT_TYPE,
                    "application/openmetrics-text; version=1.0.0; charset=utf-8",
                )],
                String::from_utf8(buffer).unwrap(),
            )
                .into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}
