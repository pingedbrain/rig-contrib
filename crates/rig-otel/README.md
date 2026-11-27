# rig-otel

OpenTelemetry for [Rig](https://github.com/0xPlaygrounds/rig) agents in one call.

Rig already describes every agent run, model call and tool call as `tracing` spans that
follow the OpenTelemetry GenAI semantic conventions. `rig-otel` exports them, and adds
what a backend needs beyond the spans:

| | |
|---|---|
| **Traces** | OTLP over HTTP to any collector (Jaeger, Grafana Tempo, Honeycomb, Langfuse, Phoenix, AWS CloudWatch), from the standard `OTEL_*` variables. |
| **Metrics** | `gen_ai.client.operation.duration` and `gen_ai.client.token.usage`, with the semantic conventions' buckets, plus tool and model-request metrics (`rig.tool.*`, `rig.model.*`), read from Rig's spans: no agent code changes. |
| **Sessions** | `set_session` gives a span and everything under it a `session.id`, which travels as W3C baggage to the services it calls. |
| **Across processes** | `inject` and `extract` carry `traceparent` and `baggage` in HTTP headers, so an agent calling another (A2A, MCP) makes one trace. |

```rust
use rig_agent::Agent;
use tracing::Instrument;

// At the start of main. Keep the guard until main returns: dropping it flushes.
fn setup() -> Result<rig_otel::Telemetry, rig_otel::InitError> {
    // OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME, OTEL_EXPORTER_OTLP_HEADERS... as usual.
    rig_otel::init()
}

// A conversation: every span under it carries session.id.
async fn converse(
    agent: Agent,
    session_id: String,
    message: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let span = tracing::info_span!("conversation");
    rig_otel::set_session(&span, session_id, None)?;
    let answer = async { agent.prompt(message).await }.instrument(span).await?;
    Ok(answer.to_string())
}

// Calling another service: it continues this trace and session.
fn call_out(request: &mut http::Request<()>) {
    rig_otel::inject(request.headers_mut());
}

// Serving a call: continue the caller's trace.
fn serve_call(span: &tracing::Span, request: &http::Request<()>) -> Result<(), tracing_opentelemetry::SetParentError> {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_parent(rig_otel::extract(request.headers()))
}
```

`init()` exports to the endpoint in `OTEL_EXPORTER_OTLP_ENDPOINT`, or to the OTLP HTTP
default `http://localhost:4318` when that is unset. Starting without a collector does not
fail: exports run in the background and fail there. `force_flush()` returns the export
error. Dropping `Telemetry` ignores it, so call `force_flush()` where delivery matters.
Metrics go to the collector's metrics endpoint: a collector that stores traces only, such
as Jaeger, rejects them, while traces still arrive.

`Telemetry::builder()` sets the service name, the span filter, logs to stderr, or your
own span and metric exporters (in-memory ones in tests), and `build()` hands you the
subscriber to install yourself.

## Metrics

| Name | Unit | Attributes | Source | Closest Strands name |
|------|------|------------|--------|----------------------|
| `gen_ai.client.operation.duration` | `s` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.tool.name`, `error.type` | Every GenAI span (`chat`, `execute_tool`, `invoke_agent`), duration of the span. OpenTelemetry GenAI semconv. | none |
| `gen_ai.client.token.usage` | `{token}` | the same, plus `gen_ai.token.type` (`input` or `output`) | Token usage recorded on a GenAI span. OpenTelemetry GenAI semconv. | none |
| `rig.tool.call_count` | `{call}` | `gen_ai.agent.name`, `gen_ai.tool.name` | One per `execute_tool` span, so denied and skipped calls count too. | `strands.tool.call_count` |
| `rig.tool.success_count` | `{call}` | `gen_ai.agent.name`, `gen_ai.tool.name` | `execute_tool` spans with `gen_ai.tool.call.outcome` `success`. | `strands.tool.success_count` |
| `rig.tool.error_count` | `{call}` | `gen_ai.agent.name`, `gen_ai.tool.name`, `error.type` | `execute_tool` spans with outcome `error`. `error.type` is `gen_ai.tool.error.type`. | `strands.tool.error_count` |
| `rig.tool.duration` | `s` | `gen_ai.agent.name`, `gen_ai.tool.name`, `error.type` on failed calls | Duration of each `execute_tool` span. | `strands.tool.duration` |
| `rig.model.request_count` | `{request}` | `gen_ai.agent.name` | One per `chat` span: one model request attempt. Retries and turns the hooks reject count too. | `strands.event_loop.cycle_count`, but not the same count (see below) |
| `rig.model.request_duration` | `s` | `gen_ai.agent.name` | Duration of each `chat` span. | `strands.event_loop.cycle_duration`, but not the same duration (see below) |

The last column names the closest metric Strands Agents emits, for reference only. Nothing
here is wired to Strands or AgentCore dashboards: the `rig.*` names are the only ones this
crate emits. Strands has no semconv equivalent for tool calls, so `rig.tool.*` comes from
Rig's `execute_tool` spans.

`rig.model.*` counts model request attempts, not completed turns. A Strands event-loop
cycle also covers the tool calls that run after its model call, so the Strands cycle
count and duration differ from these. The agent name is the span's own
`gen_ai.agent.name`, or the nearest enclosing one. Attributes stay low cardinality: no
session, message or call ids.

## AWS Bedrock AgentCore

Spans and metrics carry the instrumentation scope `opentelemetry.instrumentation.rig`,
and conversations carry `session.id`: the two things AgentCore Observability and
AgentCore Evaluations read spans by, so Rig agents show up there like agents of the
frameworks AWS instruments itself.

## Testing

`cargo test` runs a Rig agent on a scripted model with in-memory exporters: the exported
spans and their scope, the duration, token, tool and model-request metrics (one tool
call that succeeds, one that fails), a session reaching every span,
and a trace with its session crossing into another process through the headers.
