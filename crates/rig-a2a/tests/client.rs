//! A Rig agent served over A2A, reached with `RemoteAgent`: directly, and as
//! a tool of another Rig agent.
#![cfg(not(target_family = "wasm"))]

use anyhow::ensure;
use rig_a2a::a2a::TaskState;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};

async fn serve(agent: Agent) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(agent, &url).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

fn reply(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

#[tokio::test]
async fn send_answers_and_continues_a_context() -> anyhow::Result<()> {
    let model = MockCompletionModel::from_stream_turns([reply("Hello"), reply("Again")]);
    let remote = AgentBuilder::new(model.clone())
        .name("Echo Bot")
        .description("Says things back.")
        .build();
    let agent = RemoteAgent::connect(&serve(remote).await?).await?;
    ensure!(agent.card().name == "Echo Bot");

    let first = agent.send("Hi", None).await?;
    ensure!(first.text == "Hello", "first answer: {first:?}");
    ensure!(first.state == Some(TaskState::Completed));
    let context = first.context_id.clone();
    ensure!(context.is_some(), "the server opens a context");

    let second = agent.send("Once more", context.as_deref()).await?;
    ensure!(second.text == "Again", "second answer: {second:?}");
    ensure!(
        second.context_id == context,
        "the follow-up stays in the context"
    );
    Ok(())
}

#[tokio::test]
async fn a_local_agent_calls_the_remote_one_as_a_tool() -> anyhow::Result<()> {
    let remote_model = MockCompletionModel::from_stream_turns([reply("42")]);
    let remote = AgentBuilder::new(remote_model)
        .name("Oracle")
        .description("Answers questions.")
        .build();
    let tool = RemoteAgent::connect(&serve(remote).await?)
        .await?
        .into_tool();
    ensure!(
        tool.name() == "oracle",
        "tool named after the card: {}",
        tool.name()
    );

    // The local agent runs a plain (non-streaming) prompt.
    let local_model = MockCompletionModel::from_turns([
        MockTurn::tool_call(
            "call-1",
            "oracle",
            serde_json::json!({"message": "What is the answer?"}),
        ),
        MockTurn::text("The oracle says 42."),
    ]);
    let local = AgentBuilder::new(local_model.clone())
        .dynamic_tool(tool)
        .default_max_turns(3)
        .build();
    let response = local.prompt("Ask the oracle").await?;
    ensure!(response.output() == "The oracle says 42.");

    // The tool result the local model saw carries the remote answer.
    let saw_remote_answer = response.messages.iter().any(|message| {
        format!("{message:?}").contains("42") && format!("{message:?}").contains("context_id")
    });
    ensure!(
        saw_remote_answer,
        "tool result reached the model: {:#?}",
        response.messages
    );
    Ok(())
}
