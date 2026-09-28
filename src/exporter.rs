use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, StatusCode, Version, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use prometheus_client::encoding::text::{encode_eof, encode_registry};
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{Instrument, error, error_span, info, instrument, warn};

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

/// A label value escaped for the OpenMetrics text format.
///
/// `prometheus_client` writes label values verbatim between the quotes it emits
/// (see `LabelValueEncoder`), so a value containing `"`, `\` or a newline would
/// produce malformed output. Azure app display names are free text, so they have
/// to be escaped here. Revisit if `prometheus_client` ever escapes them itself,
/// as that would double-escape.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LabelValue(String);

impl<T: Into<String>> From<T> for LabelValue {
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

impl EncodeLabelValue for LabelValue {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> std::result::Result<(), std::fmt::Error> {
        for c in self.0.chars() {
            match c {
                '\\' => encoder.write_str(r"\\")?,
                '"' => encoder.write_str("\\\"")?,
                '\n' => encoder.write_str(r"\n")?,
                _ => encoder.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// A failed HTTP call to the service being scraped.
///
/// Scrapers return it inside their `anyhow` error so that the status and
/// request id can be logged as fields, not only as part of the message.
#[derive(Debug)]
pub struct UpstreamHttpError {
    pub status: u16,
    pub request_id: Option<String>,
    pub message: String,
}

impl std::fmt::Display for UpstreamHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpstreamHttpError {}

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
        let app = self.router();
        let listener = match TcpListener::bind(&self.socket).await {
            Ok(listener) => listener,
            Err(err) => {
                error!(address = %self.socket, error = %err, "Failed to bind");
                return;
            }
        };
        match listener.local_addr() {
            Ok(addr) => info!(address = %addr, "Listening"),
            Err(err) => warn!(error = %err, "Failed to get local address"),
        }
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal());
        match server.await {
            Ok(()) => info!("Exporter is shut down"),
            Err(err) => error!(error = %err, "Server error"),
        }
    }

    /// The exporter's routes. Serving it requires `ConnectInfo<SocketAddr>`,
    /// which the request log uses for the client address.
    fn router(&self) -> Router {
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
        Router::new()
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
            )
            .layer(middleware::from_fn(log_request))
    }
}

/// Log every request served, once the response is ready.
///
/// Everything logged while handling the request shares a `trace_id`, carried
/// by a span around the handler. The request details are fields of the final
/// event only: the scrape logs emitted while serving `/metrics` carry the
/// upstream Graph call's `http_status`, which must not be mixed with this
/// request's.
async fn log_request(
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let trace_id = incoming_trace_id(request.headers()).unwrap_or_else(new_trace_id);
    // At error level so that it is enabled whenever any event is: the span only
    // carries context, and a `warn` filter must not strip it from warnings.
    let span = error_span!("http_request", trace_id);
    serve_logged(client, request, next).instrument(span).await
}

async fn serve_logged(client: SocketAddr, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let http_version = http_version(request.version());
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let start = Instant::now();
    let response = next.run(request).await;
    let duration_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);

    let status = response.status();
    let outcome = if status.is_client_error() || status.is_server_error() {
        "failure"
    } else {
        "success"
    };
    info!(
        action = "http-request",
        outcome,
        method = %method,
        path,
        http_version,
        http_status = status.as_u16(),
        client_address = %client,
        user_agent,
        duration_ns,
        "{method} {path} {}",
        status.as_u16()
    );
    response
}

/// The trace id of a valid W3C `traceparent` header, so that the request's
/// logs can be matched with those of the caller.
fn incoming_trace_id(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("traceparent")?.to_str().ok()?;
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let valid = version.len() == 2
        && version != "ff"
        && trace_id.len() == 32
        && trace_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && trace_id.bytes().any(|b| b != b'0');
    valid.then(|| trace_id.to_owned())
}

/// A random W3C-style trace id: 32 lowercase hex digits, not all zero.
fn new_trace_id() -> String {
    format!("{:032x}", rand::random::<u128>().max(1))
}

/// The HTTP version the way ECS writes it in `http.version`.
fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "unknown",
    }
}

async fn status<T: PromScraper + Send + Sync + 'static>(scraper: Arc<T>) -> impl IntoResponse {
    match scraper.ready().await {
        Ok(msg) => msg.into_response(),
        Err(err) => (StatusCode::SERVICE_UNAVAILABLE, err).into_response(),
    }
}

// At error level for the same reason as the `http_request` span.
#[instrument(
    name = "scrape",
    level = "error",
    skip_all,
    fields(action = "scrape", scraper = scraper.name())
)]
async fn get_metrics<S: PromScraper + Send + Sync + 'static>(
    scraper: &S,
    success_metric: &Family<SuccessMetricLabels, Counter>,
    registry: &Registry,
) -> Response {
    let mut registries = vec![registry];
    let start = Instant::now();
    let scrape_result = scraper.scrape().await;
    let duration_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let scrape_registry;
    let outcome = match scrape_result {
        Ok(scrape_reg) => {
            info!(duration_ns, outcome = "success", "Scrape succeeded");
            scrape_registry = scrape_reg;
            registries.push(&scrape_registry);
            Outcome::Success
        }
        Err(err) => {
            let http_error = err
                .chain()
                .find_map(|e| e.downcast_ref::<UpstreamHttpError>());
            warn!(
                duration_ns,
                outcome = "failure",
                error = %format!("{err:#}"),
                http_status = http_error.map(|e| e.status),
                request_id = http_error.and_then(|e| e.request_id.as_deref()),
                "Scrape failed"
            );
            Outcome::Failure
        }
    };
    success_metric
        .get_or_create(&SuccessMetricLabels { outcome })
        .inc();
    output_metrics(registries).unwrap_or_else(|err| {
        let msg = format!("Metrics output failed: {err:#}");
        warn!(error = %format!("{err:#}"), "Metrics output failed");
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
    use crate::azure::test_support::{graph_client, scrape_registry};
    use crate::logging::test_support::LogCapture;
    use axum::http::HeaderMap;
    use serde_json::json;

    struct StubScraper;

    #[async_trait]
    impl PromScraper for StubScraper {
        async fn scrape(&self) -> Result<Registry> {
            Ok(Registry::default())
        }

        async fn ready(&self) -> std::result::Result<String, String> {
            Err("Unavailable: stub".to_string())
        }

        fn name(&self) -> &str {
            "Stub"
        }
    }

    /// Serve an exporter for `scraper` on a free local port, the way `run` does.
    async fn serve_exporter<T: PromScraper + Send + Sync + 'static>(scraper: T) -> SocketAddr {
        let app = Exporter::new("127.0.0.1:0".parse().unwrap(), scraper).router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        addr
    }

    /// A Graph that rejects every request with a 403, and the applications URL
    /// to reach it.
    async fn serve_forbidding_graph() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock_graph = Router::new().route(
            "/applications/",
            get(|headers: HeaderMap| async move {
                if headers.get(header::AUTHORIZATION).unwrap() != "Bearer test-token" {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                (
                    StatusCode::FORBIDDEN,
                    [("request-id", "abc-123")],
                    r#"{"error":{"code":"Authorization_RequestDenied","message":"Insufficient privileges to complete the operation."}}"#,
                )
                    .into_response()
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock_graph).await.unwrap() });
        format!("http://{addr}/applications/")
    }

    fn is_trace_id(value: &serde_json::Value) -> bool {
        value.as_str().is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    }

    /// Every request served is logged once, including unknown paths, with the
    /// request details as ECS fields.
    #[tokio::test]
    async fn logs_served_requests() {
        let addr = serve_exporter(StubScraper).await;

        let logs = LogCapture::start();
        let client = reqwest::Client::new();
        for path in ["/status", "/nope"] {
            client
                .get(format!("http://{addr}{path}"))
                .header(header::USER_AGENT, "Prometheus/3.0.0")
                .send()
                .await
                .unwrap();
        }

        let requests: Vec<_> = logs
            .lines()
            .into_iter()
            .filter(|line| line["event"]["action"] == "http-request")
            .collect();
        assert_eq!(requests.len(), 2, "{requests:?}");

        let status = &requests[0];
        assert_eq!(status["message"], "GET /status 503");
        assert_eq!(status["event"]["outcome"], "failure");
        assert!(status["event"]["duration"].as_u64().unwrap() > 0);
        assert_eq!(
            status["http"],
            json!({"version": "1.1", "request": {"method": "GET"}, "response": {"status_code": 503}})
        );
        assert_eq!(status["url"], json!({"path": "/status"}));
        assert_eq!(
            status["user_agent"],
            json!({"original": "Prometheus/3.0.0"})
        );
        assert_eq!(status["client"]["ip"], "127.0.0.1");
        assert!(status["client"]["port"].as_u64().unwrap() > 0);
        assert_eq!(status["log"]["level"], "info");

        let unknown = &requests[1];
        assert_eq!(unknown["message"], "GET /nope 404");
        assert_eq!(unknown["http"]["response"]["status_code"], 404);

        assert!(is_trace_id(&status["trace"]["id"]), "{status}");
        assert!(is_trace_id(&unknown["trace"]["id"]), "{unknown}");
        assert_ne!(status["trace"]["id"], unknown["trace"]["id"]);
    }

    /// A caller's W3C trace id is reused; an invalid one is replaced.
    #[tokio::test]
    async fn reuses_incoming_trace_ids() {
        let addr = serve_exporter(StubScraper).await;

        let logs = LogCapture::start();
        let client = reqwest::Client::new();
        for traceparent in [
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "garbage",
        ] {
            client
                .get(format!("http://{addr}/"))
                .header("traceparent", traceparent)
                .send()
                .await
                .unwrap();
        }

        let trace_ids: Vec<_> = logs
            .lines()
            .into_iter()
            .filter(|line| line["event"]["action"] == "http-request")
            .map(|line| line["trace"]["id"].clone())
            .collect();
        assert_eq!(trace_ids.len(), 4, "{trace_ids:?}");
        assert_eq!(trace_ids[0], "4bf92f3577b34da6a3ce929d0e0e4736");
        for generated in &trace_ids[1..] {
            assert!(is_trace_id(generated), "{generated}");
            assert_ne!(generated, "4bf92f3577b34da6a3ce929d0e0e4736");
            assert_ne!(generated, "00000000000000000000000000000000");
        }
    }

    /// Serves `/metrics` backed by a mock Graph that rejects the request, and
    /// checks that the Graph status and request id reach the log as ECS fields,
    /// on a line sharing its trace id with the request log.
    #[tokio::test]
    async fn logs_graph_errors_as_http_fields() {
        let scraper = graph_client(serve_forbidding_graph().await).await;
        let addr = serve_exporter(scraper).await;

        let logs = LogCapture::start();
        let response = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let lines = logs.lines();
        let find = |message: &str| {
            lines
                .iter()
                .find(|line| line["message"] == message)
                .unwrap_or_else(|| panic!("no {message:?} logged: {lines:?}"))
        };
        let failure = find("Scrape failed");
        assert_eq!(
            failure["http"],
            json!({"request": {"id": "abc-123"}, "response": {"status_code": 403}})
        );
        assert_eq!(
            failure["error"]["message"],
            "Graph returned 403 Forbidden: Authorization_RequestDenied: \
             Insufficient privileges to complete the operation."
        );
        assert_eq!(failure["event"]["action"], "scrape");
        assert_eq!(failure["event"]["outcome"], "failure");
        assert!(failure["event"]["duration"].as_u64().unwrap() > 0);
        assert_eq!(failure["log"]["level"], "warn");

        let request = find("GET /metrics 200");
        assert!(is_trace_id(&failure["trace"]["id"]), "{failure}");
        assert_eq!(failure["trace"]["id"], request["trace"]["id"]);
    }

    /// With `RUST_LOG=warn`, warnings keep the context of the spans they are
    /// logged in, even though those spans are not warnings themselves.
    #[tokio::test]
    async fn warn_filter_keeps_span_context() {
        let scraper = graph_client(serve_forbidding_graph().await).await;
        let addr = serve_exporter(scraper).await;

        let logs = LogCapture::with_filter("warn");
        reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();

        let lines = logs.lines();
        assert_eq!(
            lines.len(),
            1,
            "only the scrape failure is a warning: {lines:?}"
        );
        let failure = &lines[0];
        assert_eq!(failure["message"], "Scrape failed");
        assert!(is_trace_id(&failure["trace"]["id"]), "{failure}");
        assert_eq!(failure["event"]["action"], "scrape");
        assert_eq!(failure["aasm"]["scraper"], "Azure App Secrets");
    }

    /// Label values must be escaped per the OpenMetrics text format, otherwise an
    /// app whose Azure display name contains a quote breaks the whole scrape.
    #[test]
    fn escapes_label_values() {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelSet)]
        struct Labels {
            name: LabelValue,
        }

        for (raw, want) in [
            ("plain", "plain"),
            ("", ""),
            (r#"a "quoted" name"#, r#"a \"quoted\" name"#),
            (r"back\slash", r"back\\slash"),
            ("two\nlines", r"two\nlines"),
            ("\\\"\n", r#"\\\"\n"#),
            ("héllo → ok", "héllo → ok"),
        ] {
            let mut registry = <Registry>::default();
            let family = Family::<Labels, Counter>::default();
            registry.register("m", "h", family.clone());
            family.get_or_create(&Labels { name: raw.into() }).inc();

            let output = encode_registries(vec![&registry]).unwrap();
            let line = output
                .lines()
                .find(|l| l.starts_with("m_total"))
                .expect("sample line");
            assert_eq!(
                line,
                format!("m_total{{name=\"{want}\"}} 1"),
                "input: {raw:?}"
            );
        }
    }

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
            "credential_expiration_time_seconds{app_id=\"bbbb-2222\",app_name=\"Second \\\"quoted\\\" App\",key_id=\"key-2\"} 1800000000",
            "credential_expiration_time_seconds{app_id=\"cccc-3333\",app_name=\"Backslash \\\\ App\",key_id=\"key-3\"} 1900000000",
            "# EOF",
        ];
        want.sort_unstable();
        assert_eq!(got, want);

        // The terminator belongs at the very end, exactly once.
        assert!(output.ends_with("# EOF\n"));
        assert_eq!(output.matches("# EOF").count(), 1);
    }
}
