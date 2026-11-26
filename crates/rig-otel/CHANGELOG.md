# Changelog

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
