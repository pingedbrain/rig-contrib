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
mod compare;
mod dataset;
mod evaluators;
mod experiment;
mod judge;
mod multi_turn;
#[cfg(feature = "multiagent")]
mod multiagent;
mod output;
mod target;
mod telemetry;
mod timeout;

use rig_core::completion::Usage;

/// Reported usage of `input` and `output` tokens.
pub fn tokens(input: u64, output: u64) -> Usage {
    Usage::new()
        .input_tokens(input)
        .output_tokens(output)
        .total_tokens(input + output)
}

/// Pins a global default (`NoSubscriber`) once per test binary. See
/// rig-multiagent's `tests/integration/telemetry.rs` for why: without it,
/// tracing-core caches a callsite's interest from the one registered
/// dispatcher, and a thread with no subscriber can cache "never" for it.
pub(crate) fn pin_global_default() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        let _ = ::tracing::subscriber::set_global_default(
            ::tracing::subscriber::NoSubscriber::default(),
        );
    });
}
