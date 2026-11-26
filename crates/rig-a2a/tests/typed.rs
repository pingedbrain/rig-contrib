//! A typed agent served over A2A answers with JSON data, and its card says
//! what shape that data has.
#![cfg(not(target_family = "wasm"))]

use anyhow::ensure;
use rig_a2a::a2a::TaskState;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct Task {
    title: String,
    priority: String,
}

#[tokio::test]
async fn a_typed_agent_answers_with_json_its_card_describes() -> anyhow::Result<()> {
    // A typed run parses the model's final text as `Task`.
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"title": "Migrate payments to Postgres", "priority": "high"}"#,
    )]);
    let agent = AgentBuilder::new(model.clone())
        .name("Task extractor")
        .description("Turns a message into a task.")
        .build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(agent, &url).typed::<Task>().into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });

    let remote = RemoteAgent::connect(&url).await?;
    let card = remote.card();
    ensure!(
        card.default_output_modes == ["application/json"],
        "{card:?}"
    );
    ensure!(
        card.skills[0].description.contains("\"priority\""),
        "the schema is in the skill: {}",
        card.skills[0].description
    );

    let reply = remote
        .send("Urgent: migrate payments to Postgres", None)
        .await?;
    ensure!(reply.state == Some(TaskState::Completed), "{reply:?}");
    ensure!(reply.data.len() == 1, "{reply:?}");
    ensure!(reply.data[0]["title"] == "Migrate payments to Postgres");
    ensure!(reply.data[0]["priority"] == "high");
    ensure!(
        model.requests()[0].output_schema.is_some(),
        "the run asked the model for structured output"
    );
    Ok(())
}
