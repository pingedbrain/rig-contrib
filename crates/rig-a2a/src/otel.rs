//! Trace context and sessions across agents (feature `otel`): every call to
//! a remote agent carries the W3C `traceparent` and `baggage` of its
//! `a2a.call` span, and the `a2a.task` span on the other side continues that
//! trace, so both processes' spans form one trace.

use a2a::A2AError;
use a2a_client::ServiceParams;
use a2a_client::middleware::CallInterceptor;
use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// The header AWS Bedrock AgentCore Runtime passes its session id in.
const AGENTCORE_SESSION: &str = "x-amzn-bedrock-agentcore-runtime-session-id";

/// Adds the current span's trace context to every v1.0 call.
pub(crate) struct TraceContext;

#[async_trait::async_trait]
impl CallInterceptor for TraceContext {
    async fn before(&self, _method: &str, params: &mut ServiceParams) -> Result<(), A2AError> {
        let mut headers = HeaderMap::new();
        rig_otel::inject(&mut headers);
        for (name, value) in &headers {
            if let Ok(value) = value.to_str() {
                params
                    .entry(name.as_str().to_owned())
                    .or_default()
                    .push(value.to_owned());
            }
        }
        Ok(())
    }
}

/// Make a task's `span` (before it is entered) continue the caller's trace,
/// in the session AgentCore names, else in the task's A2A context.
pub(crate) fn continue_trace(span: &tracing::Span, params: &ServiceParams, context_id: &str) {
    let mut headers = HeaderMap::new();
    for (name, values) in params {
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        for value in values {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.append(name.clone(), value);
            }
        }
    }
    let session = params
        .get(AGENTCORE_SESSION)
        .and_then(|values| values.first())
        .map(String::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(context_id);
    if let Err(error) = rig_otel::set_session(span, session, Some(rig_otel::extract(&headers))) {
        // No OpenTelemetry layer installed: nothing to continue.
        tracing::debug!(%error, "the A2A task does not continue the caller's trace");
    }
}
