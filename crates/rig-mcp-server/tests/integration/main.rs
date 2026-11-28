//! The integration tests of rig-mcp-server, one module per suite, linked into
//! one binary.

mod catalog;
mod host;
mod limits;
mod mcp;
#[cfg(feature = "otel")]
mod otel;

/// Pins a global default (`NoSubscriber`) once per test binary. See
/// rig-multiagent's `tests/integration/telemetry.rs` for why: without it,
/// tracing-core caches a callsite's interest from the one registered
/// dispatcher, and a thread with no subscriber can cache "never" for it.
#[cfg(feature = "otel")]
pub(crate) fn pin_global_default() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        let _ = ::tracing::subscriber::set_global_default(
            ::tracing::subscriber::NoSubscriber::default(),
        );
    });
}
