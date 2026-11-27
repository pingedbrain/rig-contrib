//! The README flow end to end: telemetry set up, a conversation with a
//! session, and a trace that crosses a call to another process.
//!
//! The model is scripted, so no API key is needed: `MockCompletionModel` from
//! `rig_core::test_utils` is a test mock. Replace `scripted_agent` with your
//! real model to trace real calls. The "other process" is a second agent in
//! this one, reached through HTTP headers, so the trace crosses the same
//! boundary a real call would.
//!
//! Two modes, chosen by `OTEL_EXPORTER_OTLP_ENDPOINT`:
//!
//! - Unset (the default): spans and metrics go to in-memory exporters, and
//!   the spans are printed at the end with their `session.id`. Nothing is
//!   sent anywhere.
//! - Set, e.g. `http://127.0.0.1:4318`: spans and metrics go over OTLP/HTTP
//!   to that collector. If nothing listens there, the example still runs to
//!   the end: export failures are dropped silently, as in any OTLP exporter.
//!   To look at the trace in Jaeger:
//!
//! ```sh
//! docker run --rm -d -p 4318:4318 -p 16686:16686 jaegertracing/all-in-one
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_SERVICE_NAME=rig-otel-example \
//!     cargo run -p rig-otel --example console
//! # then open http://127.0.0.1:16686 and pick service rig-otel-example
//! ```
//!
//! Without a collector, run it as `cargo run -p rig-otel --example console`.

use opentelemetry::Value;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_otel::{SESSION_ID, Telemetry};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const SESSION: &str = "demo-session-1";

/// An agent whose model answers `reply` and nothing else.
fn scripted_agent(reply: &str) -> rig_agent::Agent {
    let model = MockCompletionModel::from_turns([MockTurn::text(reply)]);
    AgentBuilder::new(model).name("assistant").build()
}

/// One prompt, run inside `span` so it and its children carry the span's context.
async fn ask(
    agent: &rig_agent::Agent,
    text: &str,
    span: tracing::Span,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = async { agent.prompt(text).await }.instrument(span).await?;
    Ok(response.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let otlp = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(otlp))
}

async fn run(otlp: bool) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Telemetry. `init()` reads the OTEL_* variables; otherwise memory.
    let memory = (!otlp).then(|| {
        (
            InMemorySpanExporter::default(),
            InMemoryMetricExporter::default(),
        )
    });
    let telemetry = match &memory {
        Some((spans, metrics)) => Telemetry::builder()
            .service_name("rig-otel-example")
            .span_exporter(spans.clone())
            .metric_exporter(metrics.clone())
            .init()?,
        None => rig_otel::init()?,
    };

    // 2. A conversation: its span and everything under it carry session.id.
    let conversation = rig_otel::agent_span("assistant");
    rig_otel::set_session(&conversation, SESSION, None)?;
    let agent = scripted_agent("Hello from the scripted model.");
    let answer = ask(&agent, "Say hello.", conversation.clone()).await?;
    println!("caller: {answer}");

    // 3. A call to another service, from inside the conversation. The
    // headers carry the trace and the session; the callee continues both.
    let mut headers = http::HeaderMap::new();
    {
        let _in_call = conversation.enter();
        rig_otel::inject(&mut headers);
    }
    drop(conversation);
    let handler = tracing::info_span!("handle request");
    handler.set_parent(rig_otel::extract(&headers))?;
    let other = scripted_agent("Hello back from the other service.");
    let reply = ask(&other, "Say hello back.", handler.clone()).await?;
    drop(handler);
    println!("callee: {reply}");

    // 4. Flush, then print what was exported (memory mode only).
    if let Some((spans, _metrics)) = &memory {
        telemetry.force_flush()?;
        for span in spans.get_finished_spans()? {
            print_span(&span);
        }
    } else if let Err(err) = telemetry.force_flush() {
        // Not fatal: the collector may be absent, or lack a metrics endpoint.
        eprintln!("telemetry not fully exported: {err}");
    } else {
        println!("spans sent to OTEL_EXPORTER_OTLP_ENDPOINT");
    }
    Ok(())
}

fn print_span(span: &SpanData) {
    let session = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == SESSION_ID)
        .map(|kv| match &kv.value {
            Value::String(s) => s.to_string(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "-".to_owned());
    println!(
        "span {:<20} trace {} session {}",
        span.name,
        span.span_context.trace_id(),
        session
    );
}
