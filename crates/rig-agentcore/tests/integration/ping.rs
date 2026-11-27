//! `/ping` while work is open: `HealthyBusy` during an invocation, a spawned
//! background task or a tracked guard, `Healthy` again once it ends, and
//! `time_of_last_update` moving only on a status change.

use std::sync::Arc;
use std::time::Duration;

use rig_agent::{Agent, AgentBuilder};
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// A tool that returns when `release` is notified, or never if it is not.
struct Gate(Arc<Notify>);

impl Tool for Gate {
    const NAME: &'static str = "gate";
    type Args = Value;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Waits until released".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        self.0.notified().await;
        Ok("released".into())
    }
}

fn agent(model: MockCompletionModel, release: Arc<Notify>) -> Agent {
    AgentBuilder::new(model)
        .name("Gated")
        .tool(Gate(release))
        .default_max_turns(2)
        .build()
}

async fn serve(app: AgentCoreApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });
    format!("http://{address}")
}

async fn ping(url: &str) -> Value {
    reqwest::get(format!("{url}/ping"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn status(ping: &Value) -> &str {
    ping.get("status")
        .and_then(Value::as_str)
        .expect("a status")
}

fn since(ping: &Value) -> u64 {
    ping.get("time_of_last_update")
        .and_then(Value::as_u64)
        .expect("a time_of_last_update")
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

async fn invoke(url: &str, prompt: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, "s")
        .json(&json!({"prompt": prompt}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_slow_run_is_healthy_busy_until_it_ends() {
    let release = Arc::new(Notify::new());
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "gate", json!({})),
        MockTurn::text("done"),
    ]);
    let url = serve(AgentCoreApp::new(agent(model.clone(), release.clone()))).await;
    assert_eq!(status(&ping(&url).await), "Healthy");

    let run_url = url.clone();
    let run = tokio::spawn(async move { invoke(&run_url, "wait").await });
    wait_for_calls(&model, 1).await;
    assert_eq!(status(&ping(&url).await), "HealthyBusy");

    release.notify_one();
    let answer: Value = run.await.unwrap().json().await.unwrap();
    assert_eq!(answer.get("response"), Some(&json!("done")));
    assert_eq!(status(&ping(&url).await), "Healthy");
}

#[tokio::test]
async fn a_spawned_task_keeps_the_app_busy_until_it_finishes() {
    let app = AgentCoreApp::new(agent(
        MockCompletionModel::text("unused"),
        Arc::new(Notify::new()),
    ));
    let release = Arc::new(Notify::new());
    let waiter = release.clone();
    let url = serve(app.clone()).await;

    let task = app.spawn_task(async move { waiter.notified().await });
    assert_eq!(status(&ping(&url).await), "HealthyBusy");

    release.notify_one();
    task.await.unwrap();
    assert_eq!(status(&ping(&url).await), "Healthy");
}

#[tokio::test]
async fn a_tracked_guard_is_busy_until_dropped() {
    let app = AgentCoreApp::new(agent(
        MockCompletionModel::text("unused"),
        Arc::new(Notify::new()),
    ));
    let url = serve(app.clone()).await;

    let guard = app.track_task();
    assert_eq!(status(&ping(&url).await), "HealthyBusy");
    drop(guard);
    assert_eq!(status(&ping(&url).await), "Healthy");
}

#[tokio::test]
async fn a_run_that_times_out_releases_its_busy_state() {
    let model = MockCompletionModel::from_turns([MockTurn::tool_call("call-1", "gate", json!({}))]);
    let app = AgentCoreApp::new(agent(model.clone(), Arc::new(Notify::new())))
        .max_run_duration(Duration::from_millis(300));
    let url = serve(app).await;

    let stopped = invoke(&url, "hang").await;
    assert_eq!(stopped.status(), 504);
    assert_eq!(status(&ping(&url).await), "Healthy");
}

#[tokio::test]
async fn time_of_last_update_moves_only_when_the_status_changes() {
    let app = AgentCoreApp::new(agent(
        MockCompletionModel::text("unused"),
        Arc::new(Notify::new()),
    ));
    let url = serve(app.clone()).await;
    // Unix seconds: sleep past a second so each change is visible.
    let idle = since(&ping(&url).await);
    assert_eq!(
        since(&ping(&url).await),
        idle,
        "a repeated ping keeps the time"
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let guard = app.track_task();
    let busy = ping(&url).await;
    assert_eq!(status(&busy), "HealthyBusy");
    assert!(since(&busy) > idle, "{busy}");
    assert_eq!(
        since(&ping(&url).await),
        since(&busy),
        "still busy, same time"
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    drop(guard);
    let healthy = ping(&url).await;
    assert_eq!(status(&healthy), "Healthy");
    assert!(since(&healthy) > since(&busy), "{healthy}");
}
