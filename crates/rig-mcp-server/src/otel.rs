//! Continuing a client's trace, and the session spans are grouped by (the
//! `otel` feature).

use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use rmcp::RoleServer;
use rmcp::model::JsonObject;
use rmcp::service::RequestContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The HTTP header AWS Bedrock AgentCore Runtime names its session with.
const AGENTCORE_SESSION: &str = "x-amzn-bedrock-agentcore-runtime-session-id";

/// The W3C fields that carry a trace across processes.
const FIELDS: [&str; 3] = ["traceparent", "tracestate", "baggage"];

/// What a call continues: the client's trace context, and the AgentCore
/// session it belongs to, if any.
pub(crate) struct Incoming {
    parent: opentelemetry::Context,
    session: Option<String>,
}

impl Incoming {
    /// Read from the HTTP request headers (Streamable HTTP) first, then the
    /// request's `_meta` (any transport, stdio too), field by field.
    pub(crate) fn of(context: &RequestContext<RoleServer>) -> Self {
        let http = context
            .extensions
            .get::<Parts>()
            .map(|parts| &parts.headers);
        Self::read(http, &context.meta)
    }

    fn read(http: Option<&HeaderMap>, meta: &JsonObject) -> Self {
        let mut headers = HeaderMap::new();
        for field in FIELDS {
            let value = http
                .and_then(|headers| headers.get(field).cloned())
                .or_else(|| {
                    meta.get(field)
                        .and_then(|value| value.as_str())
                        .and_then(|value| HeaderValue::from_str(value).ok())
                });
            if let Some(value) = value {
                headers.insert(HeaderName::from_static(field), value);
            }
        }
        let session = http
            .and_then(|headers| headers.get(AGENTCORE_SESSION))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self {
            parent: rig_otel::extract(&headers),
            session,
        }
    }

    /// Make `span` continue the client's trace, in the AgentCore session if
    /// there is one, else in `fallback` (an agent's own session) if given.
    pub(crate) fn continue_in(&self, span: &tracing::Span, fallback: Option<&str>) {
        let parent = self.parent.clone();
        let result = match self.session.as_deref().or(fallback) {
            Some(session) => rig_otel::set_session(span, session, Some(parent)),
            None => span.set_parent(parent),
        };
        if let Err(failure) = result {
            tracing::debug!(%failure, "could not continue the client's trace");
        }
    }
}

#[cfg(test)]
mod tests;
