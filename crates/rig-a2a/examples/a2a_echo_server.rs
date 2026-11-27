//! A deterministic A2A agent for interoperability tests: no model, no API key.
//!
//! Every message is answered "pong", streamed in two chunks, by a scripted
//! Rig agent served with `A2aServer` (v1.0 at the root, v0.3 under `/v03`).
//!
//! ```sh
//! PORT=3000 A2A_TOKEN=secret cargo run --example a2a_echo_server
//! ```
//!
//! `PORT` defaults to 3000; with `A2A_TOKEN` set, calls need that bearer token.

use std::process::ExitCode;

use anyhow::Context as _;
use rig_a2a::A2aServer;
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

/// How many messages the script answers before the mock model runs dry.
const REPLIES: usize = 1_000;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// `PORT`, or `default` when it is not set. A value that is not a port number is an error.
fn port(default: u16) -> anyhow::Result<u16> {
    match std::env::var("PORT") {
        Ok(value) => value
            .parse()
            .with_context(|| format!("PORT must be a port number from 0 to 65535, got {value:?}")),
        Err(_) => Ok(default),
    }
}

async fn run() -> anyhow::Result<()> {
    let port = port(3000)?;
    let turns = (0..REPLIES).map(|_| {
        vec![
            MockStreamEvent::text("po"),
            MockStreamEvent::text("ng"),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    });
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(turns))
        .name("Echo")
        .description("Answers every message with pong.")
        .build();

    let url = format!("http://127.0.0.1:{port}");
    let mut server = A2aServer::new(agent, &url);
    if let Ok(token) = std::env::var("A2A_TOKEN") {
        server = server.bearer_token(token);
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not listen on 127.0.0.1:{port}"))?;
    eprintln!("echo agent ready at {url}");
    rig_a2a::axum::serve(listener, server.into_router()).await?;
    Ok(())
}
