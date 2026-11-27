//! The limits on `/invocations`: concurrent invocations, run duration and
//! request size. The scripted model's tool call hangs, so a run stays open
//! until a limit stops it.

use std::sync::Arc;
use std::time::Duration;

use rig_agent::{Agent, AgentBuilder};
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// A tool that never returns, so the run that calls it stays open.
struct Hang(Arc<Notify>);

impl Tool for Hang {
    const NAME: &'static str = "hang";
    type Args = Value;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Never returns".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        self.0.notified().await;
        Ok("unreachable".into())
    }
}

fn agent(model: MockCompletionModel) -> Agent {
    AgentBuilder::new(model)
        .name("Slow")
        .tool(Hang(Arc::new(Notify::new())))
        .default_max_turns(2)
        .build()
}

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

async fn wait_for_calls(model: &MockCompletionModel, calls: usize) {
    for _ in 0..500 {
        if model.request_count() >= calls {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the model was not called {calls} times");
}

async fn invoke(url: &str, session: &str, prompt: &str, streaming: bool) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, session)
        .json(&json!({"prompt": prompt}));
    if streaming {
        request = request.header("accept", "text/event-stream");
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn a_run_past_its_time_answers_504_and_keeps_the_history() {
    let model = MockCompletionModel::from_turns([
        MockTurn::text("first answer"),
        MockTurn::tool_call("call-1", "hang", json!({})),
        MockTurn::text("after answer"),
    ]);
    let app = AgentCoreApp::new(agent(model.clone())).max_run_duration(Duration::from_millis(300));
    let url = serve(app).await;

    let first: Value = invoke(&url, "s", "first question", false)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first.get("response"), Some(&json!("first answer")));

    let stopped = invoke(&url, "s", "hang now", false).await;
    assert_eq!(stopped.status(), 504);
    let body: Value = stopped.json().await.unwrap();
    assert_eq!(body.get("status"), Some(&json!("error")));
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("took longer than")),
        "{body}"
    );

    let after: Value = invoke(&url, "s", "after", false)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after.get("response"), Some(&json!("after answer")));
    let third_call = sent(&model, 2);
    assert!(
        third_call.contains("first question"),
        "the finished turn is kept"
    );
    assert!(
        !third_call.contains("hang now"),
        "the stopped turn is not added to the session"
    );
}

#[tokio::test]
async fn a_stopped_stream_ends_with_an_error_event() {
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::tool_call("call-1", "hang", json!({})),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let app = AgentCoreApp::new(agent(model)).max_run_duration(Duration::from_millis(300));
    let url = serve(app).await;

    let body = invoke(&url, "s", "hang now", true)
        .await
        .text()
        .await
        .unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let last = events.last().expect("an event");
    assert!(
        last.get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("took longer than")),
        "{events:?}"
    );
    assert!(events.iter().all(|event| event.get("response").is_none()));
}

#[tokio::test]
async fn invocations_beyond_the_cap_are_refused_before_the_agent_runs() {
    let model = MockCompletionModel::from_turns([MockTurn::tool_call("call-1", "hang", json!({}))]);
    let url = serve(AgentCoreApp::new(agent(model.clone())).max_concurrent_tasks(1)).await;

    // The first invocation takes the only slot and keeps it: its run hangs.
    let holder_url = url.clone();
    let holder = tokio::spawn(async move { invoke(&holder_url, "a", "hold", false).await });
    wait_for_calls(&model, 1).await;

    let refused = invoke(&url, "b", "second", false).await;
    assert_eq!(refused.status(), 429);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body.get("status"), Some(&json!("error")));
    assert_eq!(model.request_count(), 1, "the refused invocation never ran");

    holder.abort();
}

#[tokio::test]
async fn a_body_over_the_default_limit_gets_413_with_the_json_error() {
    let model = MockCompletionModel::from_turns([MockTurn::text("hi")]);
    let url = serve(AgentCoreApp::new(agent(model.clone()))).await;

    let big = "x".repeat(3 * 1024 * 1024);
    let response = invoke(&url, "s", &big, false).await;
    assert_eq!(response.status(), 413);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body.get("error"),
        Some(&json!("the request body is larger than 2097152 bytes"))
    );
    assert_eq!(model.request_count(), 0, "an oversized body never runs");
}

#[tokio::test]
async fn the_body_limit_is_an_option_and_malformed_bodies_stay_400() {
    let model = MockCompletionModel::from_turns([MockTurn::text("small")]);
    let url = serve(AgentCoreApp::new(agent(model)).max_request_bytes(1024)).await;

    let big = "x".repeat(2048);
    let response = invoke(&url, "s", &big, false).await;
    assert_eq!(response.status(), 413);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body.get("error"),
        Some(&json!("the request body is larger than 1024 bytes"))
    );

    let small: Value = invoke(&url, "s", "hi", false).await.json().await.unwrap();
    assert_eq!(small.get("response"), Some(&json!("small")));

    let malformed = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
}

#[tokio::test]
async fn a_hung_summary_is_stopped_with_the_run_and_the_history_kept() {
    let main = MockCompletionModel::from_turns([MockTurn::text("one"), MockTurn::text("two")]);
    let summarizer =
        MockCompletionModel::from_turns([MockTurn::tool_call("call-1", "hang", json!({}))]);
    let app = AgentCoreApp::new(agent(main.clone()))
        .summarize_history(agent(summarizer.clone()), 1, 2)
        .max_run_duration(Duration::from_millis(300));
    let url = serve(app).await;

    for prompt in ["first", "second"] {
        assert_eq!(invoke(&url, "s", prompt, false).await.status(), 200);
    }
    // The session now holds four messages, past the trigger, so this
    // invocation summarizes first, and the summary never returns.
    let stopped = invoke(&url, "s", "third", false).await;
    assert_eq!(stopped.status(), 504);
    assert_eq!(summarizer.request_count(), 1, "the summary was attempted");
    assert_eq!(
        main.request_count(),
        2,
        "the stopped run never reached the model"
    );
}
