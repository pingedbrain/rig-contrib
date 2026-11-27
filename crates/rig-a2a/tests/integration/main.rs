//! The integration tests, in one binary.

mod client;
mod compat_auth;
mod history;
mod limits;
mod max_turns;
mod otel;
mod rich_stream;
mod serve;
mod tracing;
mod typed;

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
