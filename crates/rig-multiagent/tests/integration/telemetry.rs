//! What swarm and graph runs tell an OpenTelemetry backend, exported in memory.

use opentelemetry::Value;
use opentelemetry::trace::Status;
use opentelemetry_sdk::metrics::InMemoryMetricExporterBuilder;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_agent::AgentBuilder;
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_multiagent::{Graph, HANDOFF_TOOL, NodeError, Swarm, node_fn};
use serde_json::json;

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

/// Keeps two dispatchers registered for the whole binary. With only one,
/// tracing-core computes a callsite's interest from the registering thread's
/// own default, so another test's span-less first hit caches "never" for
/// every thread, and a later scoped subscriber here silently misses it. With
/// two registered, interest is computed from all of them, whichever thread
/// registers the callsite.
fn pin_global_default() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        let _ =
            tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
    });
}

async fn capture<F: Future<Output = ()>>(body: impl FnOnce() -> F) -> Vec<SpanData> {
    pin_global_default();
    let spans = InMemorySpanExporter::default();
    let (telemetry, subscriber) = rig_otel::Telemetry::builder()
        .span_exporter(spans.clone())
        .metric_exporter(InMemoryMetricExporterBuilder::new().build())
        .logs(false)
        .build()
        .expect("telemetry");
    let guard = tracing::subscriber::set_default(subscriber);
    body().await;
    drop(guard);
    telemetry.force_flush().expect("flush");
    spans.get_finished_spans().expect("spans")
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| match &kv.value {
            Value::I64(n) => n.to_string(),
            other => other.to_string(),
        })
}

fn tokens(input: u64, output: u64) -> Usage {
    let mut usage = Usage::default();
    usage.input_tokens = Some(input);
    usage.output_tokens = Some(output);
    usage
}

#[tokio::test]
async fn a_swarm_run_span_carries_its_totals_and_handoffs() {
    let spans = capture(|| async {
        let first = AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::tool_call(
            "call-1",
            HANDOFF_TOOL,
            json!({"agent_name": "second", "message": "Finish it"}),
        )
        .with_usage(tokens(10, 2))]))
        .name("first")
        .build();
        let second = AgentBuilder::new(MockCompletionModel::from_turns([
            MockTurn::text("done").with_usage(tokens(20, 3))
        ]))
        .name("second")
        .build();
        let swarm = Swarm::builder()
            .agent(first)
            .agent(second)
            .build()
            .expect("swarm");
        swarm.run("Task").await.expect("run");
    })
    .await;

    let run = spans
        .iter()
        .find(|span| span.name == "swarm.run")
        .expect("a swarm.run span");
    assert_eq!(attribute(run, "swarm.agent").as_deref(), Some("second"));
    assert_eq!(attribute(run, "swarm.handoffs").as_deref(), Some("1"));
    assert_eq!(
        attribute(run, "swarm.usage.input_tokens").as_deref(),
        Some("30")
    );
    assert_eq!(
        attribute(run, "swarm.usage.output_tokens").as_deref(),
        Some("5")
    );
    let handoff = run
        .events
        .iter()
        .find(|event| event.name == "swarm.handoff")
        .expect("a handoff event");
    assert!(
        handoff
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "swarm.to" && kv.value.to_string() == "second")
    );
}

#[tokio::test]
async fn a_failed_graph_node_marks_its_span_and_the_run() {
    let spans = capture(|| async {
        let broken = node_fn(|_: String| -> Reply { Box::pin(async { Err("disk full".into()) }) });
        let graph = Graph::builder()
            .node("broken", broken)
            .build()
            .expect("graph");
        assert!(graph.run("Task").await.is_err());
    })
    .await;

    for name in ["graph.node", "graph.run"] {
        let span = spans.iter().find(|span| span.name == name).expect(name);
        assert!(
            matches!(span.status, Status::Error { .. }),
            "{name}: {:?}",
            span.status
        );
        assert_eq!(
            attribute(span, "error.type").as_deref(),
            Some("node_failed"),
            "{name}"
        );
    }
}
