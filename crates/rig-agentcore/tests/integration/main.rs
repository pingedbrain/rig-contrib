//! rig-agentcore's integration tests: one binary, one module per suite.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable
)]

mod contract;
mod history;
mod limits;
mod ping;
mod summary;
mod summary_span;

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
