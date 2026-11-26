//! The limits that keep an exposed agent from spending without bound:
//! concurrent tasks, run duration and request size.
#![cfg(not(target_family = "wasm"))]

use std::sync::Arc;
use std::time::Duration;

use anyhow::ensure;
use futures::StreamExt;
use rig_a2a::a2a::TaskState;
use rig_a2a::{A2aServer, RemoteAgent, ReplyEvent, V03_PATH};
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use tokio::sync::Notify;

/// A tool that never returns, so its run holds its slot until stopped.
struct Hang(Arc<Notify>);

impl Tool for Hang {
    const NAME: &'static str = "hang";
    type Args = serde_json::Value;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Never returns".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        self.0.notified().await;
        Ok("unreachable".into())
    }
}

/// An agent whose every run calls `hang`; `runs` scripts that many of them.
fn hanging_agent(runs: usize) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_stream_turns((0..runs).map(|_| {
        vec![
            MockStreamEvent::tool_call("call-1", "hang", serde_json::json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    let agent = AgentBuilder::new(model.clone())
        .name("Slow")
        .tool(Hang(Arc::new(Notify::new())))
        .default_max_turns(2)
        .build();
    (agent, model)
}

async fn serve(server: impl FnOnce(String) -> A2aServer) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = server(url.clone()).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

#[tokio::test]
async fn tasks_beyond_the_limit_are_rejected_as_busy_before_the_model() -> anyhow::Result<()> {
    let (agent, model) = hanging_agent(2);
    let url = serve(|url| A2aServer::new(agent, url).max_concurrent_tasks(1)).await?;
    let remote = RemoteAgent::connect(&url).await?;

    // The first task takes the only slot and keeps it.
    let mut first = remote.stream("first", None).await?;
    while let Some(event) = first.next().await {
        if event? == ReplyEvent::Status(TaskState::Working) {
            break;
        }
    }

    let second = remote.send("second", None).await?;
    ensure!(second.state == Some(TaskState::Rejected), "{second:?}");
    ensure!(second.text.contains("busy"), "{second:?}");
    ensure!(
        model.request_count() == 1,
        "the rejected task never reached the model"
    );
    Ok(())
}

#[tokio::test]
async fn a_run_past_its_time_is_stopped_and_fails() -> anyhow::Result<()> {
    let (agent, _) = hanging_agent(1);
    let url = serve(|url| A2aServer::new(agent, url).max_run_duration(Duration::from_millis(300)))
        .await?;
    let reply = RemoteAgent::connect(&url).await?.send("hi", None).await?;
    ensure!(reply.state == Some(TaskState::Failed), "{reply:?}");
    ensure!(reply.text.contains("longer than allowed"), "{reply:?}");
    Ok(())
}

#[tokio::test]
async fn oversized_requests_get_413_on_both_versions() -> anyhow::Result<()> {
    let (agent, _) = hanging_agent(0);
    let url = serve(|url| A2aServer::new(agent, url).max_request_bytes(1024)).await?;
    let big = "x".repeat(4096);
    for path in ["/jsonrpc", V03_PATH] {
        let status = reqwest::Client::new()
            .post(format!("{url}{path}"))
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"x","params":{{"pad":"{big}"}}}}"#
            ))
            .send()
            .await?
            .status();
        ensure!(status == 413, "{path}: {status}");
    }
    Ok(())
}
