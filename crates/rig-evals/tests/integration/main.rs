//! The integration tests of rig-evals, one module per suite, linked into one
//! binary. Scripted models: no keys, no network (except the AgentCore suite,
//! which skips unless pointed at a runtime).

// Tests may fail loudly, as unit tests may (see the crate root); indexing
// still goes through `.get(..)`.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable
)]

#[cfg(feature = "agentcore")]
mod agentcore;
mod dataset;
mod evaluators;
mod experiment;
mod judge;
#[cfg(feature = "multiagent")]
mod multiagent;
mod output;
mod target;
mod telemetry;

use rig_core::completion::Usage;

/// Reported usage of `input` and `output` tokens.
pub fn tokens(input: u64, output: u64) -> Usage {
    Usage::new()
        .input_tokens(input)
        .output_tokens(output)
        .total_tokens(input + output)
}
