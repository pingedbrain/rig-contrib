//! A deterministic MCP server for interoperability tests: no model, no API key.
//!
//! Serves an `echo` agent (answers "pong", streamed as "po" + "ng") and a
//! read-only `add` tool, over stdio or Streamable HTTP:
//!
//! ```sh
//! cargo run --example echo_server              # stdio
//! PORT=3400 MCP_TOKEN=secret cargo run --example echo_server -- http
//! ```

use rig_agent::AgentBuilder;
use rig_core::completion::ToolAnnotations;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use rig_mcp_server::McpServer;
use serde_json::{Value, json};

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
            args[key]
                .as_f64()
                .ok_or_else(|| ToolExecutionError::other("not a number"))
        };
        Ok(json!({"sum": n("a")? + n("b")?}))
    }
}

#[tokio::main]
async fn main() -> Result<(), rig_mcp_server::ServeError> {
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
    let mut server = McpServer::new("echo").agent(agent).tool(Add);

    if std::env::args().nth(1).as_deref() != Some("http") {
        return server.serve_stdio().await;
    }
    if let Ok(token) = std::env::var("MCP_TOKEN") {
        server = server.bearer_token(token);
    }
    let port: u16 = std::env::var("PORT").map_or(Ok(3400), |port| port.parse())?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("echo MCP server at http://127.0.0.1:{port}/mcp");
    rig_mcp_server::axum::serve(listener, server.into_router()).await?;
    Ok(())
}
