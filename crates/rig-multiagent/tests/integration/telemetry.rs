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

/// Pins a global default (`NoSubscriber`) once per test binary. The dispatch
/// that `Telemetry::build` registers is live on the test thread before
/// `set_default` runs, so with only that one registration tracing-core takes
/// its single-dispatcher path, reads this thread's default (none), and caches
/// "never" for the callsite. The second live registration turns that path off,
/// and the global is never dropped, so it stays off for the rest of the binary.
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
    // The handoff message is prompt content: its length is recorded, the text is not.
    assert_eq!(
        handoff
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "swarm.message_chars")
            .map(|kv| kv.value.to_string())
            .as_deref(),
        Some("9")
    );
    assert!(
        handoff
            .attributes
            .iter()
            .all(|kv| !kv.value.to_string().contains("Finish it")),
        "handoff text must not reach telemetry"
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
