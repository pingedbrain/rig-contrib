//! An agent with a tool and no `default_max_turns` ends its run after the
//! first model call. The remote caller is told how to raise the limit, and a
//! run that finishes in time gets no hint.
#![cfg(not(target_family = "wasm"))]

use anyhow::ensure;
use rig_a2a::a2a::TaskState;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};

/// A tool that answers at once.
struct Echo;

impl Tool for Echo {
    const NAME: &'static str = "echo";
    type Args = serde_json::Value;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Echoes its input".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        Ok("echoed".into())
    }
}

/// A model that calls `echo` once, then answers with `final_text` if asked.
fn tool_then_answer(final_text: Option<&str>) -> MockCompletionModel {
    let mut turns = vec![vec![
        MockStreamEvent::tool_call("call-1", "echo", serde_json::json!({})),
        MockStreamEvent::final_response_with_default_usage(),
    ]];
    if let Some(text) = final_text {
        turns.push(vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_default_usage(),
        ]);
    }
    MockCompletionModel::from_stream_turns(turns)
}

async fn serve(agent: Agent) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(agent, &url).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

#[tokio::test]
async fn a_tool_agent_without_a_turn_budget_tells_the_caller_how_to_raise_it() -> anyhow::Result<()>
{
    let agent = AgentBuilder::new(tool_then_answer(None))
        .name("Helper")
        .tool(Echo)
        .build();
    let remote = RemoteAgent::connect(&serve(agent).await?).await?;

    let reply = remote.send("say hi", None).await?;
    ensure!(reply.state == Some(TaskState::Failed), "{reply:?}");
    ensure!(
        reply.text.contains("turn limit (1)")
            && reply.text.contains("AgentBuilder::default_max_turns(n)"),
        "the hint names the limit and the fix: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_run_that_finishes_gets_no_turn_hint() -> anyhow::Result<()> {
    let agent = AgentBuilder::new(tool_then_answer(Some("Done.")))
        .name("Helper")
        .tool(Echo)
        .default_max_turns(2)
        .build();
    let remote = RemoteAgent::connect(&serve(agent).await?).await?;

    let reply = remote.send("say hi", None).await?;
    ensure!(reply.state == Some(TaskState::Completed), "{reply:?}");
    ensure!(reply.text == "Done.", "{reply:?}");
    ensure!(!reply.text.contains("default_max_turns"), "{reply:?}");
    Ok(())
}

#[tokio::test]
async fn a_budget_that_runs_out_says_to_raise_it() -> anyhow::Result<()> {
    // The budget is set, but the model keeps calling the tool past it.
    let model = MockCompletionModel::from_stream_turns((0..2).map(|attempt| {
        vec![
            MockStreamEvent::tool_call(format!("call-{attempt}"), "echo", serde_json::json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    let agent = AgentBuilder::new(model)
        .name("Helper")
        .tool(Echo)
        .default_max_turns(2)
        .build();
    let remote = RemoteAgent::connect(&serve(agent).await?).await?;

    let reply = remote.send("keep going", None).await?;
    ensure!(reply.state == Some(TaskState::Failed), "{reply:?}");
    ensure!(
        reply.text.contains("turn limit (2)")
            && reply
                .text
                .contains("raise AgentBuilder::default_max_turns above 2"),
        "the hint names the budget to raise: {}",
        reply.text
    );
    Ok(())
}
