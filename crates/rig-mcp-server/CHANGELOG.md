# Changelog

Versions follow Rig's: rig-mcp-server 0.47.x works with Rig 0.47.

## Unreleased

- Logs no longer print the Rig error text. A failed agent call, a failed typed call, and a failed session load or save log a short `kind` and a fixed reason, never the error text, because the text can carry provider output or prompt content. No public API or behavior changed.
- Docs: doc alias `max_concurrent_tasks` on `McpServer::max_concurrent_calls`. The root README has a table of the settings across the crates. No names or behavior changed.
- Examples `typed_tool` and `mcp_echo_server` print one line on failure (`error: could not listen on
  127.0.0.1:3400: ...`, or the `PORT` value that is not a number) instead of Rust's `Debug` output.
- Docs: the front page shows serving over Streamable HTTP with a bearer token and how an HTTP client connects; the README shows the Claude Code HTTP command and where content recording is set.
- Agents that hit their turn limit now say how to raise it: the error result names `AgentBuilder::default_max_turns(n)`. Error variants and statuses are unchanged.
- `McpServer::resource`, `binary_resource` and `prompt` offer fixed resources and prompts
  (`resources/list`, `resources/read`, `prompts/list`, `prompts/get`). The server
  advertises those capabilities only when it has some; templates, subscriptions and
  list-changed notifications are not supported.
- `into_router` docs list the duplicate resource and prompt errors, not only duplicate tools.
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
