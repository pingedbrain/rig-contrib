# Changelog

## Unreleased

- An `OTEL_EXPORTER_OTLP_ENDPOINT` (or traces or metrics endpoint) value that is not an `http` or `https` URL with a host is reported as a warning at setup, naming the variable and the value. Setup still succeeds, as before.
- Failed exports are logged as warnings naming the signal and the endpoint, at most once a minute per exporter, with a count of the failures skipped. They reach stderr with `RUST_LOG` set. The README has a "No traces showing up?" list.
- The CloudWatch exporters (`aws_cloudwatch`) time out: 10 s per request by default, set by `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` / `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` or `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds; zero is ignored), with a 5 s connect timeout. A hung endpoint no longer stalls the batch exporter or shutdown.
- README snippets are compile-checked as doctests; the conversation example awaits the prompt inside an async block before `.instrument(span)`.
- New metrics read from the same spans: `rig.tool.call_count`, `rig.tool.success_count`, `rig.tool.error_count` and `rig.tool.duration` (from `execute_tool` spans), and `rig.model.request_count` and `rig.model.request_duration` (from `chat` spans, one per model request attempt). The existing GenAI metrics are unchanged. See the README's metrics table.

## 0.47.3 (2026-11-27)

- Example `console`: the README flow with a scripted model, printing spans, or sending them to an OTLP endpoint if one is set.

Versions follow Rig's: rig-otel 0.47.x works with Rig 0.47.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.

## 0.47.0 (2026-11-26)

First release.

- `init()` and `Telemetry::builder()`: Rig's spans over OTLP, GenAI duration and token
  metrics read from them, logs to stderr, scope `opentelemetry.instrumentation.rig`.
- `set_session`: `session.id` on every span of a conversation, carried as baggage.
- `inject` and `extract`: W3C trace context and baggage in HTTP headers.
- Feature `aws`: `TelemetryBuilder::aws_cloudwatch()` exports to Amazon CloudWatch's OTLP
  endpoints, signed with SigV4 from the standard AWS credential chain.
