# Changelog

Versions follow Rig's: rig-mcp-server 0.47.x works with Rig 0.47.

## Unreleased

- README snippets are compile-checked as doctests, so the examples cannot rot.

## 0.47.3 (2026-11-27)

- `examples/typed_tool`: a typed Rig tool (arguments derived from a struct, schema
  generated with `args_schema`) served over stdio or Streamable HTTP, with a test that
  checks the schema a client sees.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.

## 0.47.0 (2026-11-26)

First release.

- `McpServer`: Rig agents, typed agents and plain Rig tools as MCP tools, on the
  official MCP Rust SDK (rmcp 3.11).
- Agents keep sessions (in memory or any rig-memory store, windowed by token budget),
  stream their text as progress notifications, stop on cancellation, and answer with
  text, images and `{answer, session_id}`; typed agents with structured content
  matching their output schema.
- Rig tool annotations carried over (readOnlyHint, destructiveHint, ...).
- stdio, Streamable HTTP (bearer token, allowed hosts) and any rmcp transport.
- Limits on concurrent calls and run duration; `mcp.agent_call` tracing spans.
- `otel` feature: calls continue the client's trace (W3C trace context from the HTTP
  headers or the request's `_meta`), and agent calls' spans carry `session.id` (the
  AgentCore Runtime session header, else the call's `session_id`), through rig-otel.
- Two tools with one name are refused with `BuildError::DuplicateTool` when the
  server is served or turned into a router.
- Keyless interop suite against the official MCP Python SDK, in CI.
