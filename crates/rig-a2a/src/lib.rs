//! Rig agents over the Agent2Agent (A2A) protocol, version 1.0, both ways:
//! serve a Rig [`Agent`](rig_agent::Agent) to any A2A client with
//! [`A2aServer`], and call any A2A agent from a Rig agent as a tool with
//! [`RemoteAgent`].
//!
//! [`A2aServer`] builds the agent card and an Axum router with the JSON-RPC
//! and HTTP+JSON bindings, streaming over server-sent events, and task get,
//! list and cancel. The protocol itself comes from the official A2A Rust SDK,
//! re-exported as [`a2a`] and [`a2a_server`].
//!
//! Each message starts a task. The reply streams into one text artifact and
//! the task completes, fails, or is canceled. Messages that share a
//! `contextId` share the agent's conversation history, which lives in memory
//! for the life of the server. [`A2aExecutor`] is the same agent as an
//! [`a2a_server::AgentExecutor`] for a handler you assemble yourself.
//!
//! ```no_run
//! use rig_a2a::A2aServer;
//! use rig_agent::AgentBuilder;
//! use rig_core::providers::openai::{self, OpenAI};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let model = OpenAI::from_env()?.completion(openai::GPT_5_4_MINI);
//! let agent = AgentBuilder::new(model).name("helper").build();
//! let app = A2aServer::new(agent, "http://localhost:3000").into_router();
//! let listener = tokio::net::TcpListener::bind("localhost:3000").await?;
//! rig_a2a::axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

// The A2A server stack (axum, tokio, hyper) runs on native targets only.
#[cfg(target_family = "wasm")]
compile_error!(
    "rig-a2a is native-only: it serves agents over HTTP with the A2A SDK's \
     axum server. Disable the `a2a` feature for wasm targets."
);

#[cfg(not(target_family = "wasm"))]
mod client;
#[cfg(not(target_family = "wasm"))]
mod executor;
#[cfg(not(target_family = "wasm"))]
mod history;
#[cfg(not(target_family = "wasm"))]
mod parts;
#[cfg(not(target_family = "wasm"))]
mod server;
#[cfg(not(target_family = "wasm"))]
mod v03;

#[cfg(not(target_family = "wasm"))]
pub use client::{
    DEFAULT_TIMEOUT, FileContent, RemoteAgent, RemoteAgentBuilder, RemoteError, RemoteFile, Reply,
    ReplyEvent,
};
#[cfg(not(target_family = "wasm"))]
pub use executor::{A2aExecutor, DEFAULT_MAX_CONCURRENT_TASKS, DEFAULT_MAX_RUN_DURATION};
#[cfg(not(target_family = "wasm"))]
pub use history::{DEFAULT_HISTORY_TOKENS, DEFAULT_MAX_CONTEXTS};
#[cfg(not(target_family = "wasm"))]
pub use server::{A2aServer, DEFAULT_MAX_REQUEST_BYTES, JSONRPC_PATH, REST_PATH};
#[cfg(not(target_family = "wasm"))]
pub use v03::V03_PATH;

/// The A2A protocol types this crate is built against.
#[cfg(not(target_family = "wasm"))]
pub use a2a;
/// The A2A client this crate is built against.
#[cfg(not(target_family = "wasm"))]
pub use a2a_client;
/// The A2A server framework this crate is built against.
#[cfg(not(target_family = "wasm"))]
pub use a2a_server;
/// The Axum version the router is built with, for `axum::serve`.
#[cfg(not(target_family = "wasm"))]
pub use axum;
/// The session stores and history windows conversations are kept with.
#[cfg(not(target_family = "wasm"))]
pub use rig_memory;

#[cfg(all(test, not(target_family = "wasm")))]
mod tests;
