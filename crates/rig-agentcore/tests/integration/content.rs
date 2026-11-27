//! Content recording on the spans: the agent's own setting applies unless the
//! app sets `record_content`, and the invocation span follows the app only.

use opentelemetry_sdk::metrics::InMemoryMetricExporterBuilder;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_agent::AgentBuilder;
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use serde_json::json;

use super::contract::serve;

const PROMPT: &str = "the secret phrase is lavender";

/// Serve an agent whose own setting is `agent_content` (None: unset) behind an
/// app whose setting is `app_content`, invoke it once, and return the spans.
async fn invoke(
    agent_content: Option<bool>,
    app_content: Option<bool>,
    streaming: bool,
) -> Vec<SpanData> {
    let spans = InMemorySpanExporter::default();
    let (telemetry, subscriber) = rig_otel::Telemetry::builder()
        .span_exporter(spans.clone())
        .metric_exporter(InMemoryMetricExporterBuilder::new().build())
        .logs(false)
        .build()
        .unwrap();
    crate::pin_global_default();
    let guard = tracing::subscriber::set_default(subscriber);

    let model = if streaming {
        MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("ok"),
            MockStreamEvent::final_response_with_default_usage(),
        ]])
    } else {
        MockCompletionModel::text("ok")
    };
    let mut builder = AgentBuilder::new(model).name("helper");
    if let Some(record) = agent_content {
        builder = builder.record_content_telemetry(record);
    }
    let mut app = AgentCoreApp::new(builder.build());
    if let Some(record) = app_content {
        app = app.record_content(record);
    }
    let url = serve(app).await;

    let accept = if streaming {
        "text/event-stream"
    } else {
        "application/json"
    };
    let body = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header("accept", accept)
        .header(SESSION_HEADER, "content-session")
        .json(&json!({"prompt": PROMPT}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("ok"), "{body}");

    drop(guard);
    telemetry.force_flush().unwrap();
    spans.get_finished_spans().unwrap()
}

/// The attribute `key` of `span`, as text.
fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// Whether any span carries `text` in an attribute: the prompt, when content
/// is recorded.
fn carries(spans: &[SpanData], text: &str) -> bool {
    spans.iter().any(|span| {
        span.attributes
            .iter()
            .any(|kv| kv.value.to_string().contains(text))
    })
}

fn invocation(spans: &[SpanData]) -> &SpanData {
    spans
        .iter()
        .find(|span| span.name.starts_with("invoke_agent"))
        .expect("invocation span")
}

#[tokio::test(flavor = "current_thread")]
async fn the_agent_setting_applies_when_the_app_is_unset() {
    for streaming in [false, true] {
        let spans = invoke(Some(true), None, streaming).await;
        assert!(
            carries(&spans, PROMPT),
            "runner spans carry content (streaming: {streaming})"
        );
        // The invocation span has no agent setting to follow, so it stays off.
        assert_eq!(
            attribute(invocation(&spans), "gen_ai.task.input"),
            None,
            "streaming: {streaming}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_app_setting_false_overrides_the_agent() {
    for streaming in [false, true] {
        let spans = invoke(Some(true), Some(false), streaming).await;
        assert!(
            !carries(&spans, PROMPT),
            "no content anywhere (streaming: {streaming})"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_app_setting_true_records_content_with_the_agent_off() {
    for streaming in [false, true] {
        let spans = invoke(Some(false), Some(true), streaming).await;
        let span = invocation(&spans);
        assert_eq!(
            attribute(span, "gen_ai.task.input").as_deref(),
            Some(PROMPT),
            "streaming: {streaming}"
        );
        assert_eq!(
            attribute(span, "gen_ai.task.output").as_deref(),
            Some("ok"),
            "streaming: {streaming}"
        );
        assert!(
            carries(&spans, PROMPT),
            "the app's true overrides the agent's false (streaming: {streaming})"
        );
    }
}
