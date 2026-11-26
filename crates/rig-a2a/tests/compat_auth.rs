//! The v0.3 surface, the bearer token and the client timeout, over loopback.
#![cfg(not(target_family = "wasm"))]

use std::sync::Arc;
use std::time::Duration;

use anyhow::ensure;
use rig_a2a::a2a::TaskState;
use rig_a2a::{A2aServer, RemoteAgent, RemoteError, V03_PATH};
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use tokio::sync::Notify;

/// Bind first, then build the server at the bound URL.
async fn serve_agent(agent: Agent, token: Option<&str>) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut server = A2aServer::new(agent, &url);
    if let Some(token) = token {
        server = server.bearer_token(token);
    }
    let app = server.into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

fn echo(replies: &[&str]) -> Agent {
    let turns = replies.iter().map(|text| {
        vec![
            MockStreamEvent::text(*text),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    });
    AgentBuilder::new(MockCompletionModel::from_stream_turns(turns))
        .name("Echo")
        .description("Says things back.")
        .build()
}

#[tokio::test]
async fn a_v03_client_talks_to_the_v03_surface() -> anyhow::Result<()> {
    let url = serve_agent(echo(&["Hello", "Again"]), None).await?;

    // The v0.3 card is published under the prefix, in v0.3 shape.
    let card: serde_json::Value =
        reqwest::get(format!("{url}{V03_PATH}/.well-known/agent-card.json"))
            .await?
            .json()
            .await?;
    ensure!(card["protocolVersion"] == "0.3.0" && card["url"] == format!("{url}{V03_PATH}"));

    let agent = RemoteAgent::connect(&format!("{url}{V03_PATH}")).await?;
    ensure!(agent.card().name == "Echo");
    let first = agent.send("Hi", None).await?;
    ensure!(
        first.text == "Hello" && first.state == Some(TaskState::Completed),
        "{first:?}"
    );
    let second = agent.send("More", first.context_id.as_deref()).await?;
    ensure!(
        second.text == "Again" && second.context_id == first.context_id,
        "{second:?}"
    );
    Ok(())
}

#[tokio::test]
async fn v03_streams_as_server_sent_events() -> anyhow::Result<()> {
    let url = serve_agent(echo(&["Hel"]), None).await?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "message/stream",
        "params": {"message": {"kind": "message", "role": "user", "messageId": "m1",
            "parts": [{"kind": "text", "text": "Hi"}]}}
    });
    let text = reqwest::Client::new()
        .post(format!("{url}{V03_PATH}"))
        .json(&body)
        .send()
        .await?
        .text()
        .await?;
    let events: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()))
        .collect::<Result<_, _>>()?;
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e["result"]["kind"].as_str())
        .collect();
    ensure!(kinds.first() == Some(&"task"), "{kinds:?}");
    ensure!(kinds.contains(&"artifact-update"), "{kinds:?}");
    let last = events.last().unwrap();
    ensure!(last["id"] == 7, "frames answer the request id: {last}");
    ensure!(
        last["result"]["kind"] == "status-update"
            && last["result"]["status"]["state"] == "completed"
            && last["result"]["final"] == true,
        "{last}"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_v03_methods_are_json_rpc_errors() -> anyhow::Result<()> {
    let url = serve_agent(echo(&[]), None).await?;
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("{url}{V03_PATH}"))
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "nope", "params": {}}))
        .send()
        .await?
        .json()
        .await?;
    ensure!(response["error"]["code"] == -32601, "{response}");
    Ok(())
}

#[tokio::test]
async fn the_bearer_token_guards_calls_but_not_cards() -> anyhow::Result<()> {
    let url = serve_agent(echo(&["v1", "v03"]), Some("s3cret")).await?;

    // Cards stay public and declare the scheme.
    for path in ["", V03_PATH] {
        let card: serde_json::Value =
            reqwest::get(format!("{url}{path}/.well-known/agent-card.json"))
                .await?
                .error_for_status()?
                .json()
                .await?;
        ensure!(
            card["securitySchemes"]["bearer"].is_object(),
            "{path}: {card}"
        );
    }

    // Without the token: 401 on both versions.
    for path in ["", V03_PATH] {
        let anonymous = RemoteAgent::connect(&format!("{url}{path}")).await?;
        let refused = anonymous.send("Hi", None).await;
        ensure!(
            refused.is_err(),
            "{path}: call without a token answered: {refused:?}"
        );
    }
    let wrong = RemoteAgent::builder(url.clone())
        .bearer_token("guess")
        .connect()
        .await?;
    ensure!(
        wrong.send("Hi", None).await.is_err(),
        "a wrong token is refused"
    );

    // With it: answered.
    for (path, expected) in [("", "v1"), (V03_PATH, "v03")] {
        let agent = RemoteAgent::builder(format!("{url}{path}"))
            .bearer_token("s3cret")
            .connect()
            .await?;
        let reply = agent.send("Hi", None).await?;
        ensure!(reply.text == expected, "{path}: {reply:?}");
    }
    Ok(())
}

/// A tool that never returns, so the remote run never ends.
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

#[tokio::test]
async fn a_silent_remote_times_out() -> anyhow::Result<()> {
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::tool_call("call-1", "hang", serde_json::json!({})),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let agent = AgentBuilder::new(model)
        .name("Slow")
        .tool(Hang(Arc::new(Notify::new())))
        .default_max_turns(2)
        .build();
    let url = serve_agent(agent, None).await?;

    let remote = RemoteAgent::builder(url)
        .timeout(Duration::from_millis(300))
        .connect()
        .await?;
    match remote.send("Hi", None).await {
        Err(RemoteError::Timeout(after)) => ensure!(after == Duration::from_millis(300)),
        other => anyhow::bail!("expected a timeout, got {other:?}"),
    }
    Ok(())
}
