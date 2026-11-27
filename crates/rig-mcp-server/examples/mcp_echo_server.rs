//! A deterministic MCP server for interoperability tests: no model, no API key.
//!
//! Serves an `echo` agent (answers "pong", streamed as "po" + "ng"), a
//! read-only `add` tool, a text and a binary resource, and an `explain` prompt,
//! over stdio or Streamable HTTP:
//!
//! ```sh
//! cargo run --example mcp_echo_server              # stdio
//! PORT=3400 MCP_TOKEN=secret cargo run --example mcp_echo_server -- http
//! ```

use anyhow::Context as _;
use rig_agent::AgentBuilder;
use rig_core::completion::ToolAnnotations;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use rig_mcp_server::McpServer;
use rmcp::model::{PromptArgument, PromptMessage, Role};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::ExitCode;

/// How many calls the scripted agent answers.
const REPLIES: usize = 1_000;

struct Add;

impl Tool for Add {
    const NAME: &'static str = "add";
    type Args = Value;
    type Output = Value;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Add two numbers".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]})
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }
    }

    async fn call(&self, _: &mut ToolContext, args: Value) -> Result<Value, Self::Error> {
        let n = |key: &str| {
            args.get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| ToolExecutionError::other("not a number"))
        };
        Ok(json!({"sum": n("a")? + n("b")?}))
    }
}

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

async fn run() -> anyhow::Result<()> {
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
    let mut server = McpServer::new("echo")
        .agent(agent)
        .tool(Add)
        .resource("memo://intro", "intro", "text/markdown", "# Hello")
        .binary_resource(
            "blob://logo",
            "logo",
            "application/octet-stream",
            [1_u8, 2, 3],
        )
        .prompt(
            "explain",
            "Explain a topic.",
            vec![PromptArgument::new("topic").with_required(true)],
            |args: &HashMap<String, String>| {
                let topic = args.get("topic").map_or("", String::as_str);
                vec![PromptMessage::new_text(
                    Role::User,
                    format!("Explain {topic} in one paragraph."),
                )]
            },
        );

    if std::env::args().nth(1).as_deref() != Some("http") {
        return server
            .serve_stdio()
            .await
            .map_err(|error| anyhow::anyhow!(error));
    }
    if let Ok(token) = std::env::var("MCP_TOKEN") {
        server = server.bearer_token(token);
    }
    let port = port(3400)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not listen on 127.0.0.1:{port}"))?;
    eprintln!("echo MCP server at http://127.0.0.1:{port}/mcp");
    rig_mcp_server::axum::serve(listener, server.into_router()?).await?;
    Ok(())
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
