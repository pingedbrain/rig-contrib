//! Bad and dead OTLP endpoints are reported, not silent.

use std::net::TcpListener;
use std::sync::{Mutex, PoisonError};

use opentelemetry::trace::{Tracer, TracerProvider as _};
use rig_otel::Telemetry;

/// The warnings rig-otel emits, recorded by the global default.
pub(crate) mod warnings {
    use std::sync::Mutex;

    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::Context;

    static RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The recorded warnings that contain `needle`. Tests run in parallel and
    /// share this buffer, so each one filters by a string unique to it.
    pub(crate) fn containing(needle: &str) -> Vec<String> {
        RECORDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|message| message.contains(needle))
            .cloned()
            .collect()
    }

    pub(crate) struct Recorder;

    struct Message(String);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl<S: Subscriber> Layer<S> for Recorder {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let meta = event.metadata();
            if *meta.level() == Level::WARN && meta.target().starts_with("rig_otel") {
                let mut message = Message(String::new());
                event.record(&mut message);
                RECORDED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message.0);
            }
        }
    }
}

/// The process environment is shared, so the tests that set OTLP variables
/// take this lock.
static ENV: Mutex<()> = Mutex::new(());

/// Build telemetry with `OTEL_EXPORTER_OTLP_ENDPOINT` set to `endpoint`.
fn build_with_endpoint(endpoint: &str) -> Telemetry {
    let _lock = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: `ENV` serialises the tests that touch this variable, and the
    // exporters read it only while `build` runs, under the same lock.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint) };
    let built = Telemetry::builder().logs(false).build();
    // SAFETY: as above.
    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    built.expect("setup succeeds with a bad endpoint").0
}

#[test]
fn an_invalid_endpoint_is_reported_and_setup_still_succeeds() {
    crate::pin_global_default();
    let _telemetry = build_with_endpoint("not-a-url");

    let warned = warnings::containing("OTEL_EXPORTER_OTLP_ENDPOINT=not-a-url");
    assert_eq!(warned.len(), 1, "{warned:?}");
}

#[test]
fn an_unreachable_endpoint_warns_once_naming_the_endpoint_and_signal() {
    crate::pin_global_default();
    // A port nothing listens on: bind, note the port, release it.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free local port")
        .port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let telemetry = build_with_endpoint(&endpoint);

    let tracer = telemetry.tracer_provider().tracer("endpoint-test");
    for _ in 0..3 {
        drop(tracer.start("export me"));
        assert!(telemetry.tracer_provider().force_flush().is_err());
    }

    let warned = warnings::containing(&endpoint);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("OTLP traces export"), "{warned:?}");
}
