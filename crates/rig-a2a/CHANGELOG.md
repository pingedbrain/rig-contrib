# Changelog

Versions follow Rig's: rig-a2a 0.47.x works with Rig 0.47.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.
- Same version as the other crates again.

## 0.47.1 (2026-11-26)

- Update the official A2A Rust SDK to a2a-server-lf 0.6 and a2a-client-lf 0.3 (a2a-pb
  0.4). The re-exported `a2a_server` and `a2a_client` are those versions. The SDK's
  previous protobuf crate wrote into its own sources while building, which failed on
  docs.rs, so 0.47.0 has no documentation there.

## 0.47.0 (2026-11-26)

First release.

- Serve a Rig agent over A2A v1.0 (JSON-RPC, HTTP+JSON, SSE streaming, task get, list
  and cancel) and v0.3 (JSON-RPC under `/v03`, for Strands Agents 1.x and other v0.3
  peers), on the official A2A Rust SDK. Server adapted from 0xPlaygrounds/rig#2820.
- `A2aServer::typed::<T>()`: answer with JSON data and declare its schema in the card.
- A failed task says why in terms the caller can act on (rate limited, credentials
  rejected, provider unavailable, out of turns), without the provider's error text.
  The model's images become file parts.
- Conversations in memory or in any rig-memory store, windowed by token budget.
- Limits: concurrent tasks, run duration, request size; bearer token auth.
- `RemoteAgent`: call any A2A agent, v1.0 or v0.3, streaming with an idle timeout;
  replies keep text, data and files; as a Rig tool, the model gets a JSON summary
  plus any image.
- `a2a.task` tracing spans parenting Rig's own spans; `a2a.call` spans around calls.
- Feature `otel`: calls carry W3C trace context and the remote task continues the
  trace, with `session.id` from AgentCore's session header or the A2A context.
- Keyless interop suite against Strands Agents 1.66 (both streaming modes) and the
  official A2A Python SDK 1.x, in CI.
