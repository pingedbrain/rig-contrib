//! The client side of A2A: a local Rig agent that calls a remote A2A agent as
//! one of its tools.
//!
//! The remote agent is served from this process on 127.0.0.1, on a free port,
//! and reached over HTTP like any other peer. The calling agent has no opening
//! hours of its own, so it has to delegate the question.
//!
//! No key: with neither `RIG_MODEL` nor `OPENROUTER_MODEL` set, both agents run
//! on scripted models (Rig's mocks, no network). The run still goes through the
//! whole path: the planner calls the remote tool over HTTP, and the example
//! checks that the remote agent received the call before it exits.
//!
//! ```sh
//! cargo run -p rig-a2a --example remote_tool
//! ```
//!
//! The model is read from the environment for a live run:
//!
//! - `RIG_MODEL=opencode-go/<model>` runs on OpenCode Go, with the key in
//!   `OPENCODE_GO_API_KEY`.
//! - `RIG_MODEL=<id>` runs on OpenRouter, with the key in `OPENROUTER_API_KEY`.
//!   `OPENROUTER_MODEL` is used when `RIG_MODEL` is unset.
//!
//! On OpenCode Go:
//!
//! ```sh
//! set -a; . /path/to/rig/.env; set +a
//! RIG_MODEL=opencode-go/glm-5.3 cargo run -p rig-a2a --example remote_tool
//! ```
//!
//! On OpenRouter:
//!
//! ```sh
//! set -a; . /path/to/rig/.env; set +a
//! RIG_MODEL=deepseek/deepseek-chat cargo run -p rig-a2a --example remote_tool
//! ```

use std::env;

use anyhow::{Context, Result, ensure};
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::AgentBuilder;
use rig_core::DynModel;
use rig_core::operation::Completion;
use rig_core::providers::{opencode, openrouter};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use serde_json::json;

const OPENING_TIME: &str = "07:45";
const QUESTION: &str = "When does the Harbor Library open on Tuesday? Ask the library desk.";
/// The remote agent's card name, as the tool name the planner sees.
const REMOTE_TOOL: &str = "harbor_library_desk";
/// The desk's scripted reply. It reaches the planner only through the tool result.
const DESK_REPLY: &str = "The Harbor Library opens at 07:45 on Tuesdays.";

/// A live model from the environment, with a label for the output. `None` when
/// neither `RIG_MODEL` nor `OPENROUTER_MODEL` is set.
fn live_model_from_env() -> Result<Option<(DynModel<Completion>, String)>> {
    let Ok(id) = env::var("RIG_MODEL").or_else(|_| env::var("OPENROUTER_MODEL")) else {
        return Ok(None);
    };
    if let Some(model) = id.strip_prefix("opencode-go/") {
        let key = env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY is not set")?;
        let client = opencode::go_new(key);
        return Ok(Some((
            client.completion(model).into(),
            format!("OpenCode Go {model}"),
        )));
    }
    let client = openrouter::from_env()?;
    Ok(Some((client.chat(&id).into(), format!("OpenRouter {id}"))))
}

#[tokio::main]
async fn main() -> Result<()> {
    // The scripted mocks are kept so the run can check the calls they received.
    let (desk_model, planner_model, label, scripted) = match live_model_from_env()? {
        Some((model, label)) => (model.clone(), model, label, None),
        None => {
            // The A2A server streams its replies, so the desk's script is a stream turn.
            let desk = MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::text(DESK_REPLY),
                MockStreamEvent::final_response_with_default_usage(),
            ]]);
            let planner = MockCompletionModel::from_turns([
                MockTurn::tool_call("call-1", REMOTE_TOOL, json!({"message": QUESTION})),
                MockTurn::text(format!(
                    "According to the Harbor Library desk: {DESK_REPLY}"
                )),
            ]);
            (
                desk.clone().into(),
                planner.clone().into(),
                "scripted (set RIG_MODEL or OPENROUTER_MODEL for a live run)".to_owned(),
                Some((desk, planner.clone())),
            )
        }
    };
    println!("model: {label}");

    // The remote side: an ordinary Rig agent behind the A2A server.
    let desk = AgentBuilder::new(desk_model)
        .name("Harbor Library desk")
        .description("Knows the opening hours of the Harbor Library.")
        .preamble(format!(
            "You work at the Harbor Library front desk. The library opens at {OPENING_TIME} \
             on Tuesdays. Answer in one short sentence."
        ))
        .build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(desk, &url).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });

    // The calling side: the remote agent becomes a tool named after its card.
    let remote = RemoteAgent::connect(&url).await?;
    println!("remote: {} at {url}", remote.card().name);
    let planner = AgentBuilder::new(planner_model)
        .name("Trip planner")
        .preamble(
            "You plan local trips. You do not know any library's opening hours: ask the \
             remote agent tool, then answer using only what it says.",
        )
        .dynamic_tool(remote.into_tool()?)
        // Rig's default is one turn, which stops right after the tool call, before the
        // model has read the answer. Two turns let it read the tool result and reply.
        .default_max_turns(4)
        .build();

    let response = planner.prompt(QUESTION).await?;
    println!("{}", response.output());
    if let Some((desk, planner)) = scripted {
        ensure!(
            desk.request_count() == 1,
            "the remote agent was called {} times, expected once",
            desk.request_count()
        );
        // The planner's second request must carry the tool result, so the desk's reply.
        let requests = planner.requests();
        let second = requests
            .get(1)
            .context("the planner made no second request after the tool call")?;
        ensure!(
            serde_json::to_string(second)?.contains(DESK_REPLY),
            "the planner's second request does not carry the desk's reply"
        );
    }
    ensure!(
        response.output().contains(OPENING_TIME),
        "the answer does not carry the remote opening time {OPENING_TIME}"
    );
    Ok(())
}
