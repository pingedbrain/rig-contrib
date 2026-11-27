//! rig-otel's integration tests: one binary, one module per suite.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable
)]

#[cfg(feature = "aws")]
mod aws;
mod otel;
