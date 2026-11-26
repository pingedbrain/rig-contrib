use axum::http::{HeaderMap, HeaderValue};
use opentelemetry::trace::TraceContextExt;
use rmcp::model::JsonObject;
use serde_json::json;

use super::Incoming;

const FROM_HTTP: &str = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";
const FROM_META: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn trace_id(incoming: &Incoming) -> String {
    incoming.parent.span().span_context().trace_id().to_string()
}

fn meta() -> JsonObject {
    json!({"traceparent": FROM_META})
        .as_object()
        .cloned()
        .unwrap()
}

#[test]
fn meta_carries_the_trace_when_there_is_no_http() {
    let incoming = Incoming::read(None, &meta());
    assert_eq!(trace_id(&incoming), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(incoming.session, None);
}

#[test]
fn http_headers_win_over_meta_and_name_the_agentcore_session() {
    let mut headers = HeaderMap::new();
    headers.insert("traceparent", HeaderValue::from_static(FROM_HTTP));
    headers.insert(
        "x-amzn-bedrock-agentcore-runtime-session-id",
        HeaderValue::from_static("agentcore-1"),
    );
    let incoming = Incoming::read(Some(&headers), &meta());
    assert_eq!(trace_id(&incoming), "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(incoming.session.as_deref(), Some("agentcore-1"));
}

#[test]
fn nothing_incoming_is_no_trace() {
    let incoming = Incoming::read(None, &JsonObject::new());
    assert!(!incoming.parent.span().span_context().is_valid());
}
