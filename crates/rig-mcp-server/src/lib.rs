//! Serve [Rig](https://github.com/0xPlaygrounds/rig) agents and tools over the
//! [Model Context Protocol](https://modelcontextprotocol.io), so any MCP client
//! (Claude Code, Cursor, Zed, Claude Desktop, another agent) can use them.
//!
//! - An [`agent`](McpServer::agent) becomes a tool that takes a message and an
//!   optional `session_id`, keeps the conversation, streams its text as
//!   progress notifications, and stops when the client cancels.
//! - A [`typed_agent`](McpServer::typed_agent) answers with structured content
//!   matching its output schema.
//! - A plain Rig [`tool`](McpServer::tool) is exposed as itself, with its
//!   annotations, and runs without a model.
//!
//! Built on the official MCP Rust SDK, [`rmcp`].
//!
//! ```no_run
//! use rig_mcp_server::{McpServer, ServeError};
//!
//! async fn serve(agent: rig_agent::Agent) -> Result<(), ServeError> {
//!     // A local client launches this binary and talks to it over stdio.
//!     McpServer::new("my-agents").agent(agent).serve_stdio().await
//! }
//! ```
//!
//! Over Streamable HTTP the same server answers at `/mcp`. With a bearer token,
//! calls without it get HTTP 401:
//!
//! ```no_run
//! use rig_mcp_server::McpServer;
//!
//! async fn serve_http(server: McpServer, token: &str) -> Result<(), Box<dyn std::error::Error>> {
//!     let app = server.bearer_token(token).into_router()?;
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//!     rig_mcp_server::axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!
//! An HTTP client connects to `http://127.0.0.1:3000/mcp` and sends the token on
//! every request, as `Authorization: Bearer <token>`. For Claude Code:
//! `claude mcp add --transport http my-agents http://127.0.0.1:3000/mcp --header "Authorization: Bearer <token>"`.
//!
//! Prompts and replies go on the trace only when the agent records them, with
//! `AgentBuilder::record_content_telemetry(true)`. It is off by default, as in Rig.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::unreachable
    )
)]

mod catalog;
mod content;
mod history;
#[cfg(feature = "otel")]
mod otel;
mod server;
mod transport;

pub use history::{DEFAULT_HISTORY_TOKENS, DEFAULT_MAX_CONTEXTS};
pub use server::{BuildError, DEFAULT_MAX_CONCURRENT_CALLS, DEFAULT_MAX_RUN_DURATION, McpServer};
pub use transport::{DEFAULT_MAX_REQUEST_BYTES, MCP_PATH, ServeError};

/// The Axum version [`McpServer::into_router`] builds with, for `axum::serve`.
pub use axum;
/// The session stores and history windows agent sessions are kept with.
pub use rig_memory;
/// The MCP SDK this crate is built against.
pub use rmcp;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme_doctests {}
