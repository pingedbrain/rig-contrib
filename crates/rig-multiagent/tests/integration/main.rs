//! The integration tests of rig-multiagent, one module per suite, linked into
//! one binary. Scripted models: no keys, no network.

// Tests may fail loudly, as unit tests may (see the crate root); indexing
// still goes through `.get(..)`.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable
)]

mod cache_key;
mod durable;
mod graph;
mod handoff;
mod human;
mod resume;
mod run_state;
mod stream;
mod swarm;
mod telemetry;
