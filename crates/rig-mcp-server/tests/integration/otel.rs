//! With the `otel` feature, a call continues the client's trace (from the
//! request's `_meta` or its HTTP headers) and its spans carry a `session.id`.

use std::collections::HashMap;

use anyhow::{Context, ensure};
use opentelemetry::trace::{SpanId, TraceId};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_mcp_server::McpServer;
use rmcp::ServiceExt;
use rmcp::model::RequestMetaObject;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;

use crate::mcp::{Add, call, connect, streaming_agent};

const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const PARENT: &str = "00f067aa0ba902b7";

fn traceparent() -> String {
    format!("00-{TRACE}-{PARENT}-01")
}

/// Run `body` with telemetry exporting spans to memory; the finished spans.
async fn capture<F: Future<Output = anyhow::Result<()>>>(
    body: impl FnOnce() -> F,
) -> anyhow::Result<Vec<SpanData>> {
    let spans = InMemorySpanExporter::default();
    let (telemetry, subscriber) = rig_otel::Telemetry::builder()
        .span_exporter(spans.clone())
        .metric_exporter(opentelemetry_sdk::metrics::InMemoryMetricExporter::default())
        .logs(false)
        .build()?;
    let guard = tracing::subscriber::set_default(subscriber);
    body().await?;
    drop(guard);
    telemetry.force_flush()?;
    Ok(spans.get_finished_spans()?)
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// The span named `name`, or with that `gen_ai.operation.name`.
fn span<'a>(spans: &'a [SpanData], name: &str) -> anyhow::Result<&'a SpanData> {
    spans
        .iter()
        .find(|span| {
            span.name == name || attribute(span, "gen_ai.operation.name").as_deref() == Some(name)
        })
        .with_context(|| {
            let names: Vec<_> = spans.iter().map(|span| &span.name).collect();
            format!("no `{name}` span in {names:?}")
        })
}

/// `names` spans are in the client's trace, in session `session`.
fn in_trace(spans: &[SpanData], names: &[&str], session: &str) -> anyhow::Result<()> {
    let trace = TraceId::from_hex(TRACE)?;
    for name in names {
        let span = span(spans, name)?;
        ensure!(span.span_context.trace_id() == trace, "{name}: {span:?}");
        ensure!(
            attribute(span, rig_otel::SESSION_ID).as_deref() == Some(session),
            "{name}: {:?}",
            span.attributes
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_continues_the_trace_in_its_meta() -> anyhow::Result<()> {
    let spans = capture(|| async {
        let (agent, _) = streaming_agent("Helper", &[&["Hi."]]);
        let (client, _) = connect(McpServer::new("test").agent(agent)).await?;
        let mut params = call("helper", json!({"message": "hi", "session_id": "conv-7"}));
        let mut meta = RequestMetaObject::new();
        meta.set_traceparent(traceparent());
        params.meta = Some(meta);
        let result = client.call_tool(params).await?;
        ensure!(result.is_error != Some(true), "{result:?}");
        Ok(())
    })
    .await?;

    in_trace(&spans, &["mcp.agent_call", "chat"], "conv-7")?;
    let agent_call = span(&spans, "mcp.agent_call")?;
    ensure!(
        agent_call.parent_span_id == SpanId::from_hex(PARENT)?,
        "the client's span is the parent: {agent_call:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn http_headers_carry_the_trace_and_the_agentcore_session() -> anyhow::Result<()> {
    let spans = capture(|| async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let (agent, _) = streaming_agent("Helper", &[&["Hi."]]);
        let app = McpServer::new("test")
            .agent(agent)
            .tool(Add)
            .into_router()?;
        tokio::spawn(async move { rig_mcp_server::axum::serve(listener, app).await });

        let headers = HashMap::from([
            (
                http::HeaderName::from_static("traceparent"),
                http::HeaderValue::from_str(&traceparent())?,
            ),
            (
                http::HeaderName::from_static("x-amzn-bedrock-agentcore-runtime-session-id"),
                http::HeaderValue::from_static("agentcore-session-1"),
            ),
        ]);
        let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
        let client = ().serve(StreamableHttpClientTransport::from_config(config)).await?;
        let answer = client
            .call_tool(call("helper", json!({"message": "hi"})))
            .await?;
        ensure!(answer.is_error != Some(true), "{answer:?}");
        let sum = client
            .call_tool(call("add", json!({"a": 1, "b": 2})))
            .await?;
        ensure!(sum.is_error != Some(true), "{sum:?}");
        Ok(())
    })
    .await?;

    in_trace(
        &spans,
        &["mcp.agent_call", "chat", "mcp.tool_call"],
        "agentcore-session-1",
    )
}
