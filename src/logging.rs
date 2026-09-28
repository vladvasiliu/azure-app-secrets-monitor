//! Logs as JSON lines following the Elastic Common Schema (ECS).
//!
//! Call sites use plain field names (`error`, `duration_ns`, ...) and this
//! formatter maps them to their ECS location in [`insert_field`]. Fields
//! without an ECS equivalent are kept under the `aasm.` namespace, as ECS
//! recommends for custom fields.

use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::fmt;
use std::net::SocketAddr;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

static CUSTOM_NAMESPACE: &str = "aasm";
static EVENT_DATASET: &str = "azure_app_secrets_monitor.log";

pub fn init() {
    tracing_subscriber::fmt()
        .fmt_fields(JsonFields::new())
        .event_format(EcsFormat)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

pub struct EcsFormat;

impl<S> FormatEvent<S, JsonFields> for EcsFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut doc = Map::new();
        insert_path(
            &mut doc,
            "@timestamp",
            Utc::now()
                .to_rfc3339_opts(SecondsFormat::Micros, true)
                .into(),
        );
        insert_path(
            &mut doc,
            "log.level",
            metadata.level().as_str().to_lowercase().into(),
        );
        insert_path(&mut doc, "log.logger", metadata.target().into());
        insert_path(&mut doc, "service.name", env!("CARGO_PKG_NAME").into());
        insert_path(
            &mut doc,
            "service.version",
            env!("CARGO_PKG_VERSION").into(),
        );
        insert_path(&mut doc, "event.dataset", EVENT_DATASET.into());

        // Span fields apply to every event inside the span. Going from the root
        // lets inner spans, and then the event itself, override outer values.
        let mut fields = Map::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let Some(recorded) = extensions.get::<FormattedFields<JsonFields>>() else {
                    continue;
                };
                if let Ok(Value::Object(span_fields)) = serde_json::from_str(recorded) {
                    fields.extend(span_fields);
                }
            }
        }
        event.record(&mut FieldVisitor(&mut fields));

        for (name, value) in fields {
            insert_field(&mut doc, &name, value);
        }

        let line = serde_json::to_string(&doc).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

/// Place a call-site field at its ECS location.
fn insert_field(doc: &mut Map<String, Value>, name: &str, value: Value) {
    let path = match name {
        "message" => "message",
        "action" => "event.action",
        "outcome" => "event.outcome",
        "error" => "error.message",
        "http_status" => "http.response.status_code",
        "request_id" => "http.request.id",
        // ECS durations are in nanoseconds.
        "duration_ns" => "event.duration",
        "address" => match value.as_str().and_then(|s| s.parse::<SocketAddr>().ok()) {
            Some(addr) => {
                insert_path(doc, "server.ip", addr.ip().to_string().into());
                insert_path(doc, "server.port", addr.port().into());
                return;
            }
            None => "server.address",
        },
        _ => return insert_custom(doc, name, value),
    };
    insert_path(doc, path, value);
}

fn insert_custom(doc: &mut Map<String, Value>, name: &str, value: Value) {
    insert_path(doc, &format!("{CUSTOM_NAMESPACE}.{name}"), value);
}

/// Insert `value` at a dotted `path`, creating nested objects on the way.
///
/// Nested objects rather than dotted keys, so the document means the same thing
/// whether or not the shipper expands dots.
fn insert_path(map: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            map.insert(path.to_owned(), value);
        }
        Some((head, rest)) => {
            let child = map.entry(head).or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            if let Value::Object(child) = child {
                insert_path(child, rest, value);
            }
        }
    }
}

struct FieldVisitor<'a>(&'a mut Map<String, Value>);

impl FieldVisitor<'_> {
    fn insert(&mut self, field: &Field, value: Value) {
        self.0.insert(field.name().to_owned(), value);
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value.into());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, value.to_string().into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, format!("{value:?}").into());
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber::DefaultGuard;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Records what the ECS formatter writes on the current thread, for as long
    /// as it is alive. On a current-thread runtime that covers async tests too.
    pub(crate) struct LogCapture {
        buffer: Buffer,
        _guard: DefaultGuard,
    }

    impl LogCapture {
        pub(crate) fn start() -> Self {
            let buffer = Buffer::default();
            let writer = buffer.clone();
            let subscriber = tracing_subscriber::fmt()
                .fmt_fields(JsonFields::new())
                .event_format(EcsFormat)
                .with_writer(move || writer.clone())
                .finish();
            Self {
                buffer,
                _guard: tracing::subscriber::set_default(subscriber),
            }
        }

        pub(crate) fn lines(&self) -> Vec<Value> {
            let output = String::from_utf8(self.buffer.0.lock().unwrap().clone()).unwrap();
            output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::LogCapture;
    use super::*;
    use tracing::{info_span, warn};

    fn capture(f: impl FnOnce()) -> Value {
        let logs = LogCapture::start();
        f();
        let mut lines = logs.lines();
        assert_eq!(lines.len(), 1, "expected one log line: {lines:?}");
        lines.remove(0)
    }

    #[test]
    fn maps_fields_to_ecs() {
        let mut doc = capture(|| {
            let _span = info_span!("scrape", action = "scrape", scraper = "Test").entered();
            warn!(
                duration_ns = 12_345_u64,
                outcome = "failure",
                error = "boom",
                http_status = 403_u16,
                request_id = "abc-123",
                page = 2_u32,
                "Scrape failed"
            );
        });

        let timestamp = doc.as_object_mut().unwrap().remove("@timestamp").unwrap();
        assert!(timestamp.as_str().unwrap().ends_with('Z'));
        assert_eq!(
            doc,
            serde_json::json!({
                "message": "Scrape failed",
                "log": {"level": "warn", "logger": module_path!()},
                "service": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")},
                "event": {
                    "dataset": EVENT_DATASET,
                    "action": "scrape",
                    "outcome": "failure",
                    "duration": 12_345,
                },
                "error": {"message": "boom"},
                "http": {"request": {"id": "abc-123"}, "response": {"status_code": 403}},
                "aasm": {"scraper": "Test", "page": 2},
            })
        );
    }

    #[test]
    fn splits_socket_addresses() {
        let addr: SocketAddr = "[::]:9912".parse().unwrap();
        let doc = capture(|| tracing::info!(address = %addr, "Listening"));
        assert_eq!(doc["server"], serde_json::json!({"ip": "::", "port": 9912}));
    }
}
