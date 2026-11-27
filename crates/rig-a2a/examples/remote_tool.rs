//! The client side of A2A: a local Rig agent that calls a remote A2A agent as
//! one of its tools.
//!
//! The remote agent is served from this process on 127.0.0.1, on a free port,
//! and reached over HTTP like any other peer. The calling agent has no opening
//! hours of its own, so it has to delegate the question.
//!
//! The model is read from the environment:
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

const OPENING_TIME: &str = "07:45";
const QUESTION: &str = "When does the Harbor Library open on Tuesday? Ask the library desk.";

/// The model both agents run on, and a label for the output.
fn model_from_env() -> Result<(DynModel<Completion>, String)> {
    let id = env::var("RIG_MODEL")
        .or_else(|_| env::var("OPENROUTER_MODEL"))
        .context("set RIG_MODEL, for example RIG_MODEL=opencode-go/glm-5.3")?;
    if let Some(model) = id.strip_prefix("opencode-go/") {
        let key = env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY is not set")?;
        let client = opencode::go_new(key);
        return Ok((
            client.completion(model).into(),
            format!("OpenCode Go {model}"),
        ));
    }
    let client = openrouter::from_env()?;
    Ok((client.chat(&id).into(), format!("OpenRouter {id}")))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (model, label) = model_from_env()?;
    println!("model: {label}");

    // The remote side: an ordinary Rig agent behind the A2A server.
    let desk = AgentBuilder::new(model.clone())
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
    let planner = AgentBuilder::new(model)
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
    ensure!(
        response.output().contains(OPENING_TIME),
        "the answer does not carry the remote opening time {OPENING_TIME}"
    );
    Ok(())
}
