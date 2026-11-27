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

// A conversation: the agent run and every span under it carry session.id.
async fn converse(
    agent: Agent,
    session_id: String,
    message: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let span = rig_otel::agent_span(agent.name().unwrap_or("agent"));
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

Use `agent_span` for the outer span of a run, as above. It is named `invoke_agent <agent>`
and carries `gen_ai.operation.name = invoke_agent` and `gen_ai.agent.name`, which is what
GenAI backends look for. Rig creates its own `invoke_agent` span only when no span is
enabled around the run. Inside an enabled outer span, such as `info_span!("conversation")`,
Rig adopts that span as the run's, so no `invoke_agent` span is exported. With `agent_span`
the run's span is `invoke_agent`, but Rig does not record the run's prompt, answer or usage
on it.

`init()` exports to the endpoint in `OTEL_EXPORTER_OTLP_ENDPOINT`, or to the OTLP HTTP
default `http://localhost:4318` when that is unset. Starting without a collector does not
fail: exports run in the background and fail there. `force_flush()` returns the export
error. Dropping `Telemetry` ignores it, so call `force_flush()` where delivery matters.
Metrics go to the collector's metrics endpoint: a collector that stores traces only, such
as Jaeger, rejects them, while traces still arrive.

`Telemetry::builder()` sets the service name, the span filter, logs to stderr, or your
own span and metric exporters (in-memory ones in tests), and `build()` hands you the
subscriber to install yourself.

## No traces showing up?

- **Check the endpoint value.** `OTEL_EXPORTER_OTLP_ENDPOINT` (or the traces or metrics
  variable) must be an `http` or `https` URL with a host. Anything else, such as
  `not-a-url`, is reported at startup, and setup still succeeds without a collector.
- **Turn on logs.** Export failures are warnings, and they reach stderr only when
  `RUST_LOG` is set: `RUST_LOG=warn` shows them, plus the OpenTelemetry SDK's own error
  lines. Each warning names the signal (`traces` or `metrics`) and the endpoint. At most
  one a minute is logged per exporter, with a count of the ones skipped.
- **Check the collector answers.** `curl -v <endpoint>/v1/traces` should get an HTTP
  response of any status. "Connection refused" means nothing listens there. A timeout
  means it is unreachable from this machine.
- **Check the signal.** A collector that takes traces only, such as Jaeger, rejects
  metrics. Traces still arrive, so a `metrics` warning alone does not explain missing
  traces.
- **Flush before exit.** Dropping `Telemetry` flushes, but ignores export errors. Call
  `force_flush()` where delivery matters.

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
