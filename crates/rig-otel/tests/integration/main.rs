//! rig-otel's integration tests: one binary, one module per suite.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable,
    clippy::indexing_slicing
)]

#[cfg(feature = "otlp")]
mod endpoint;
mod otel;

/// Pins a global default once per test binary. See
/// rig-multiagent's `tests/integration/telemetry.rs` for why: without it,
/// tracing-core caches a callsite's interest from the one registered
/// dispatcher, and a thread with no subscriber can cache "never" for it.
///
/// The global default also records the warnings rig-otel emits from its
/// exporter threads, where a thread-local `set_default` does not reach: see
/// [`endpoint::warnings`].
pub(crate) fn pin_global_default() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        let registry = ::tracing_subscriber::Registry::default();
        #[cfg(feature = "otlp")]
        let registry = ::tracing_subscriber::layer::SubscriberExt::with(
            registry,
            endpoint::warnings::Recorder,
        );
        let _ = ::tracing::subscriber::set_global_default(registry);
    });
}
