//! Sessions: one `session.id` on every span of a conversation.

use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::Span as _;
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use tracing_opentelemetry::{OpenTelemetrySpanExt, SetParentError};

/// The attribute, and baggage entry, a session id travels as.
pub const SESSION_ID: &str = "session.id";

/// Make `span` (before anything runs in it) the start of session `id` in
/// this process: it and every span under it get the `session.id` attribute,
/// and calls made under it carry the session to other processes, in the
/// `baggage` header [`inject`](crate::inject) writes.
///
/// `parent` is what the span continues: the context [`extract`](crate::extract)
/// read from an incoming request, or `None` to start a new trace.
pub fn set_session(
    span: &tracing::Span,
    id: impl Into<String>,
    parent: Option<Context>,
) -> Result<(), SetParentError> {
    let parent = parent.unwrap_or_default();
    // Keep the baggage the caller sent; only the session is this process's.
    let mut entries: Vec<KeyValue> = parent
        .baggage()
        .iter()
        .filter(|(key, _)| key.as_str() != SESSION_ID)
        .map(|(key, (value, _))| KeyValue::new(key.clone(), value.clone()))
        .collect();
    entries.push(KeyValue::new(SESSION_ID, id.into()));
    let cx = parent.with_baggage(entries);
    span.set_parent(cx)
}

/// Copies the session in a span's context into its attributes, as each span
/// starts.
#[derive(Debug)]
pub(crate) struct SessionSpanProcessor;

impl SpanProcessor for SessionSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        if let Some(id) = cx.baggage().get(SESSION_ID) {
            span.set_attribute(KeyValue::new(SESSION_ID, id.clone()));
        }
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
        Ok(())
    }
}
