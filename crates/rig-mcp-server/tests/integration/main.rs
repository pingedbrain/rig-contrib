//! The integration tests of rig-mcp-server, one module per suite, linked into
//! one binary.

mod catalog;
mod mcp;
#[cfg(feature = "otel")]
mod otel;
