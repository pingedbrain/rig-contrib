# rig-otel

OpenTelemetry for [Rig](https://github.com/0xPlaygrounds/rig) agents in one call.

Rig already describes every agent run, model call and tool call as `tracing` spans that
follow the OpenTelemetry GenAI semantic conventions. `rig-otel` exports them, and adds
what a backend needs beyond the spans:

| | |
|---|---|
| **Traces** | OTLP over HTTP to any collector (Jaeger, Grafana Tempo, Honeycomb, Langfuse, Phoenix, AWS CloudWatch), from the standard `OTEL_*` variables. |
| **Metrics** | `gen_ai.client.operation.duration` and `gen_ai.client.token.usage`, with the semantic conventions' buckets, read from Rig's spans: no agent code changes. |
| **Sessions** | `set_session` gives a span and everything under it a `session.id`, which travels as W3C baggage to the services it calls. |
| **Across processes** | `inject` and `extract` carry `traceparent` and `baggage` in HTTP headers, so an agent calling another (A2A, MCP) makes one trace. |

```rust
// OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME, OTEL_EXPORTER_OTLP_HEADERS... as usual.
let _telemetry = rig_otel::init()?; // flushes when dropped, at the end of main

// A conversation: every span under it carries session.id.
let span = tracing::info_span!("conversation");
rig_otel::set_session(&span, session_id, None)?;
agent.prompt(message).instrument(span).await?;

// Calling another service: it continues this trace and session.
rig_otel::inject(request.headers_mut());

// Serving a call: continue the caller's trace.
span.set_parent(rig_otel::extract(request.headers()))?;
```

`Telemetry::builder()` sets the service name, the span filter, logs to stderr, or your
own span and metric exporters (in-memory ones in tests), and `build()` hands you the
subscriber to install yourself.

## AWS Bedrock AgentCore

Spans and metrics carry the instrumentation scope `opentelemetry.instrumentation.rig`,
and conversations carry `session.id`: the two things AgentCore Observability and
AgentCore Evaluations read spans by, so Rig agents show up there like agents of the
frameworks AWS instruments itself.

## Testing

`cargo test` runs a Rig agent on a scripted model with in-memory exporters: the exported
spans and their scope, the duration and token metrics, a session reaching every span,
and a trace with its session crossing into another process through the headers.
