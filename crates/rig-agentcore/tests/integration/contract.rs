//! The AgentCore Runtime HTTP contract, served for a Rig agent on scripted
//! models and called over real HTTP.

use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
use opentelemetry_sdk::metrics::InMemoryMetricExporterBuilder;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use rig_agent::AgentBuilder;
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use serde_json::{Value, json};

async fn serve(app: AgentCoreApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });
    format!("http://{address}")
}

fn sent(model: &MockCompletionModel, request: usize) -> String {
    let requests = model.requests();
    serde_json::to_string(&requests.get(request).expect("request").chat_history).unwrap()
}

#[tokio::test]
async fn ping_reports_healthy() {
    let agent = AgentBuilder::new(MockCompletionModel::text("hi")).build();
    let url = serve(AgentCoreApp::new(agent)).await;
    let body: Value = reqwest::get(format!("{url}/ping"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body.get("status"), Some(&json!("Healthy")));
    assert!(
        body.get("time_of_last_update").is_some_and(Value::is_u64),
        "{body}"
    );
    assert_eq!(
        body.as_object().map(|object| object.len()),
        Some(2),
        "{body}"
    );
}

#[tokio::test]
async fn a_session_keeps_its_conversation_across_invocations() {
    let model = MockCompletionModel::from_turns([
        MockTurn::text("Hi Ana."),
        MockTurn::text("Your name is Ana."),
    ]);
    let agent = AgentBuilder::new(model.clone()).build();
    let url = serve(AgentCoreApp::new(agent)).await;
    let client = reqwest::Client::new();
    let invoke = |prompt: &'static str| {
        client
            .post(format!("{url}/invocations"))
            .header(SESSION_HEADER, "session-1")
            .json(&json!({"prompt": prompt}))
            .send()
    };

    let first: Value = invoke("I am Ana").await.unwrap().json().await.unwrap();
    let second: Value = invoke("What is my name?")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(first.get("response"), Some(&json!("Hi Ana.")));
    assert_eq!(second.get("response"), Some(&json!("Your name is Ana.")));
    assert!(
        sent(&model, 1).contains("I am Ana"),
        "the second call carries the first"
    );
}

#[tokio::test]
async fn sessions_do_not_share_conversations() {
    let model = MockCompletionModel::from_turns([MockTurn::text("one"), MockTurn::text("two")]);
    let agent = AgentBuilder::new(model.clone()).build();
    let url = serve(AgentCoreApp::new(agent)).await;
    let client = reqwest::Client::new();
    for (session, prompt) in [("a", "secret for a"), ("b", "hello from b")] {
        client
            .post(format!("{url}/invocations"))
            .header(SESSION_HEADER, session)
            .json(&json!({"prompt": prompt}))
            .send()
            .await
            .unwrap();
    }
    assert!(!sent(&model, 1).contains("secret for a"));
}

#[tokio::test]
async fn the_reply_streams_as_server_sent_events() {
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::text("Hel"),
        MockStreamEvent::text("lo."),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let agent = AgentBuilder::new(model).build();
    let url = serve(AgentCoreApp::new(agent)).await;

    let body = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header("accept", "text/event-stream")
        .header(SESSION_HEADER, "s")
        .json(&json!({"prompt": "Say hello"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert_eq!(
        events,
        [
            json!({"text": "Hel"}),
            json!({"text": "lo."}),
            json!({"response": "Hello."})
        ]
    );
}

#[tokio::test]
async fn a_body_without_a_prompt_is_refused() {
    let agent = AgentBuilder::new(MockCompletionModel::text("hi")).build();
    let url = serve(AgentCoreApp::new(agent)).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .json(&json!({"input": "no prompt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test(flavor = "current_thread")]
async fn an_invocation_continues_the_trace_and_names_the_session() {
    let spans = InMemorySpanExporter::default();
    let (telemetry, subscriber) = rig_otel::Telemetry::builder()
        .span_exporter(spans.clone())
        .metric_exporter(InMemoryMetricExporterBuilder::new().build())
        .logs(false)
        .build()
        .unwrap();
    crate::pin_global_default();
    let guard = tracing::subscriber::set_default(subscriber);
    let agent = AgentBuilder::new(MockCompletionModel::text("ok"))
        .name("helper")
        .build();
    let url = serve(AgentCoreApp::new(agent).record_content(true)).await;
    let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
    let caller = SpanContext::new(
        trace,
        SpanId::from_hex("b7ad6b7169203331").unwrap(),
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    );

    reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, "agentcore-session-9")
        .header(
            "traceparent",
            format!("00-{}-{}-01", caller.trace_id(), caller.span_id()),
        )
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    drop(guard);
    telemetry.force_flush().unwrap();

    let finished = spans.get_finished_spans().unwrap();
    let invocation = finished
        .iter()
        .find(|span| span.name.starts_with("invoke_agent"))
        .expect("invocation span");
    assert_eq!(invocation.span_context.trace_id(), trace);
    assert_eq!(invocation.parent_span_id, caller.span_id());
    // What AgentCore Evaluations reads from the run's span.
    let attribute = |key: &str| {
        invocation
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    assert_eq!(invocation.name, "invoke_agent helper");
    assert_eq!(
        attribute("gen_ai.operation.name").as_deref(),
        Some("invoke_agent")
    );
    assert_eq!(attribute("gen_ai.agent.name").as_deref(), Some("helper"));
    assert_eq!(attribute("gen_ai.task.input").as_deref(), Some("hi"));
    assert_eq!(attribute("gen_ai.task.output").as_deref(), Some("ok"));
    assert_eq!(
        attribute("agentcore.invocation.agent_response").as_deref(),
        Some("ok")
    );
    for span in &finished {
        let session = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "session.id")
            .map(|kv| kv.value.to_string());
        assert_eq!(
            session.as_deref(),
            Some("agentcore-session-9"),
            "{}",
            span.name
        );
    }
}
