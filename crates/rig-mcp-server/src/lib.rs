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
//! use rig_mcp_server::McpServer;
//! # async fn run(agent: rig_agent::Agent) -> Result<(), rig_mcp_server::ServeError> {
//! // A local client launches this binary and talks to it over stdio.
//! McpServer::new("my-agents").agent(agent).serve_stdio().await
//! # }
//! ```

#![forbid(unsafe_code)]
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

mod content;
mod history;
mod server;
mod transport;

pub use history::{DEFAULT_HISTORY_TOKENS, DEFAULT_MAX_CONTEXTS};
pub use server::{BuildError, DEFAULT_MAX_CONCURRENT_CALLS, DEFAULT_MAX_RUN_DURATION, McpServer};
pub use transport::{MCP_PATH, ServeError};

/// The Axum version [`McpServer::into_router`] builds with, for `axum::serve`.
pub use axum;
/// The session stores and history windows agent sessions are kept with.
pub use rig_memory;
/// The MCP SDK this crate is built against.
pub use rmcp;
