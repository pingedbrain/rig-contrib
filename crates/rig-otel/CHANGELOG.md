# Changelog

## Unreleased

- The CloudWatch exporters (`aws_cloudwatch`) time out: 10 s per request by default, set by `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` / `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` or `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds), with a 5 s connect timeout. A hung endpoint no longer stalls the batch exporter or shutdown. New `aws_cloudwatch_at` takes the region, credentials, base URL and timeout directly.
- README snippets are compile-checked as doctests; the conversation example awaits the prompt inside an async block before `.instrument(span)`.

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
