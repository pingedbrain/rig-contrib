# rig-mcp-server

Serve [Rig](https://github.com/0xPlaygrounds/rig) agents and tools over the
[Model Context Protocol](https://modelcontextprotocol.io), so any MCP client can use them:
Claude Code, Cursor, Zed, Claude Desktop, or another agent. Built on the official MCP
Rust SDK, [`rmcp`](https://crates.io/crates/rmcp).

Rig ships an MCP *client* (`rig-rmcp`, Rig agents using MCP tools). This crate is the
other direction: Rig agents *as* MCP tools.

```rust
use rig_agent::Agent;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use rig_mcp_server::{McpServer, ServeError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize, JsonSchema)]
struct Ticket { title: String, priority: String }

/// Adds two numbers. A plain Rig tool: it runs without a model.
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
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
    }

    async fn call(&self, _: &mut ToolContext, args: Value) -> Result<Value, Self::Error> {
        Ok(json!({"sum": args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0)}))
    }
}

async fn serve(research_agent: Agent, triager: Agent) -> Result<(), ServeError> {
    McpServer::new("my-agents")
        .agent(research_agent)            // a Rig agent, as a tool
        .typed_agent::<Ticket>(triager)   // answers with structured content
        .tool(Add)                        // a plain Rig tool, no model
        .serve_stdio()                    // for a client that launches this binary
        .await?;
    Ok(())
}
```

## What a client sees

| Exposed | As an MCP tool |
|---|---|
| `agent(agent)` | Named after the agent (made provider-safe), described by its description. Takes `message` and an optional `session_id`. Answers with its text and any images it made, plus `{answer, session_id}` as structured content; pass the `session_id` back to continue the conversation. |
| `typed_agent::<T>(agent)` | Same input; runs as `prompt_typed::<T>` and answers with the `T` as structured content, declared by the tool's `outputSchema` (the JSON schema of `T`). |
| `tool(t)` / `dynamic_tool(t)` | The Rig tool as itself: name, description, parameters and annotations (`readOnlyHint`, `destructiveHint`, ...). Runs without a model; its failure is an error result the model reads. A lone JSON object it returns is also structured content. |

While an agent writes, its text streams to the client as **progress notifications**
(when the client asks for progress). When the client **cancels** a call, the agent run
stops.

An agent with tools needs `AgentBuilder::default_max_turns(n)` with `n >= 2`. Without it the run stops after its first model call, and the error says so.

## Resources and prompts

Besides tools, a server can offer fixed **resources** (`resources/list` and
`resources/read`) and **prompts** (`prompts/list` and `prompts/get`). A server that has
none of either does not advertise those capabilities.

```rust
use std::collections::HashMap;

use rig_mcp_server::McpServer;
use rig_mcp_server::rmcp::model::{PromptArgument, PromptMessage, Role};

fn docs_server() -> McpServer {
    McpServer::new("docs")
        .resource("docs://style", "style", "text/markdown", "Use short sentences.")
        .prompt(
            "review",
            "Review a change for a reader.",
            vec![PromptArgument::new("diff").with_required(true)],
            |args: &HashMap<String, String>| {
                let diff = args.get("diff").map_or("", String::as_str);
                vec![PromptMessage::new_text(Role::User, format!("Review this change:\n{diff}"))]
            },
        )
}
```

- `resource(uri, name, mime_type, text)` offers text. `binary_resource(uri, name,
  mime_type, bytes)` offers bytes, sent base64-encoded.
- `prompt(name, description, arguments, render)` offers a prompt. `render` gets the
  arguments the client passed, all strings keyed by name, and returns the messages. A
  call without a required argument gets an invalid-params error.

Resource templates, resource subscriptions, list-changed notifications and completion
for prompt arguments are not supported; their capabilities are not advertised.

## Transports

```rust
use rig_mcp_server::{McpServer, ServeError};

// stdio: a local client launches the binary. Stdout carries the protocol,
// so log to stderr only.
async fn over_stdio(server: McpServer) -> Result<(), ServeError> {
    server.serve_stdio().await
}

// Streamable HTTP at /mcp, as an Axum router.
async fn over_http(
    server: McpServer,
    token: &str,
    listener: tokio::net::TcpListener,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = server.bearer_token(token).into_router()?;
    axum::serve(listener, app).await?;
    Ok(())
}

// Any rmcp transport, such as a reader and writer pair or a socket.
async fn over_transport(server: McpServer) -> Result<(), ServeError> {
    server.serve((tokio::io::stdin(), tokio::io::stdout())).await
}
```

Over HTTP the endpoint answers only to `localhost`, `127.0.0.1` and `::1` by default,
which keeps a local server safe from DNS rebinding; set `allowed_hosts` to the public
host name when deploying. With `bearer_token`, calls without the token get HTTP 401.
A request body over `max_request_bytes` (4 MiB by default) gets HTTP 413.

### In Claude Code

```sh
claude mcp add my-agents -- /path/to/your/binary
```

Over HTTP, with the token from `bearer_token`:

```sh
claude mcp add --transport http my-agents http://127.0.0.1:3000/mcp --header "Authorization: Bearer $MY_AGENTS_TOKEN"
```

## Sessions and limits

Agent sessions live in memory by default (at most 1000, oldest forgotten first), or in any
[`rig-memory`](https://crates.io/crates/rig-memory) store with `history_store` (`DirStore`
on disk, `rig-sqlite`, `rig-postgres`), so they survive restarts. Each call sends the
model only the newest messages that fit `history_window` (32k tokens by default). A
`session_id` must be 1-128 of `[A-Za-z0-9_-]`.

Every call to an agent spends model tokens, so a server bounds them:

| Knob | Default | Beyond it |
|---|---|---|
| `max_concurrent_calls` | 32 | an error result saying the server is busy, before any model call |
| `max_run_duration` | 10 min | the run is stopped, an error result |
| `bearer_token` (HTTP) | none | HTTP 401 |

Each agent call runs in an `mcp.agent_call` tracing span (with `mcp.session_id`); Rig's
spans for the model calls and tools nest under it.

## Tracing

With the `otel` feature (through [`rig-otel`](https://crates.io/crates/rig-otel), no exporters of its own), a
call continues the client's trace, so the client, the `mcp.agent_call` span and Rig's
model and tool spans under it make one trace. The W3C `traceparent`, `tracestate` and
`baggage` are read from the HTTP request headers (Streamable HTTP), else from the
request's `_meta` (any transport, stdio too; the SEP-414 convention). Plain tools run in an
`mcp.tool_call` span that continues the trace the same way.

Every span of an agent call carries `session.id`: the
`X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header when AWS Bedrock AgentCore Runtime
sends one, else the call's own `session_id`. Export the spans with `rig_otel::init()`.

```toml
rig-mcp-server = { version = "0.47", features = ["otel"] }
```

## Content recording

Prompts and replies go on the agent's spans only when it records content. It is off by
default, as in Rig. Set it on the agent you pass in with
`AgentBuilder::record_content_telemetry(true)`.

## Testing

`cargo test` drives the server with the official `rmcp` client in memory: listing,
schemas and annotations, plain tools, agent sessions, streamed progress, typed answers,
cancellation, time limits, resources and prompts (listing, reads, rendering, missing
arguments, capabilities), HTTP with and without the token, and a Rig agent using the
served tools through `rig-rmcp` (Rig to MCP to Rig).

`interop/run.sh` checks the server against an independent implementation, the official
MCP Python SDK, over stdio and Streamable HTTP, keyless (`cargo run --example
mcp_echo_server`). CI runs both, plus MSRV and `cargo-deny`.

## Not supported

MCP resource templates, resource subscriptions, list-changed notifications, prompt and
resource completion, sampling and elicitation; the SEP-2663 tasks extension
(long calls run to completion, reporting progress); OAuth (bearer only).
