# Changelog

Versions follow Rig's: rig-a2a 0.47.x works with Rig 0.47.

## Unreleased

- Client limits on what a remote agent sends. The agent card is read up to 1 MiB and refused beyond that. A v0.3 event stream is split into events in one pass, and an event longer than 16 MiB ends the stream with an error instead of growing the buffer. A v0.3 error code outside the `i32` range becomes -32603 instead of wrapping.
- Added push notifications, off by default. `A2aServer::push_notifications(true)` turns them on and the card declares `pushNotifications: true`; `push_config_store(store)` also turns them on; `push_allowed_hosts(hosts)` only limits them. Webhook URLs must be `https` and, if an allowlist is set, on it; the SDK refuses private, loopback and link-local targets, including through DNS and redirects. `allow_unguarded_push_urls()` (local tests only) drops the https and address checks. Push needs a `bearer_token`, or it stays off with an error logged. Caps: 10 configs per task, 10,000 saved per server since start; configs of tasks purged by `terminal_task_limit` are dropped when the total is reached. With push off, a config set is refused with `PushNotificationNotSupported`, as before.
- Added a test that a second client resubscribes to a running task (`tasks/resubscribe`); it passes with no code change.
- rig-a2a now depends on `async-trait` directly (it was optional under `otel`).
- Docs: doc alias `max_sessions` on `A2aServer::max_contexts` and `A2aExecutor::max_contexts`. The root README has a table of the settings across the crates. No names or behavior changed.
- Examples `a2a_serve` and `a2a_echo_server` print one line on failure (`error: could not listen on
  localhost:3000: ...`, or the `PORT` value that is not a number) instead of Rust's `Debug` output.
- Docs: the front page and README show calling a remote agent directly (`RemoteAgent::send`) and as a tool, and where content recording is set.
- Agents that hit their turn limit now say how to raise it: the failed task names `AgentBuilder::default_max_turns(n)`. Error variants and statuses are unchanged.
- Example `serve` is renamed `a2a_serve`: it clashed with the `serve` example of rig-agentcore. Run it with `cargo run --example a2a_serve`.
- Example `remote_tool` runs without a key: with neither `RIG_MODEL` nor `OPENROUTER_MODEL` set, both agents use scripted models and the run checks that the remote agent was called.
- README snippets are compile-checked as doctests; the `DirStore` example uses `DirStore::open`.
- Added `A2aServer::terminal_task_limit(n)`: the default in-memory task store keeps at most `n`
  finished tasks and purges the oldest first. A purged task answers `GetTask` with
  `TaskNotFound`; running tasks are never purged. A purged task can return if a late update
  reaches it. The default stays unlimited, so existing
  servers behave as before.

## 0.47.3 (2026-11-27)

- Example `remote_tool`: a local Rig agent calls an in-process A2A agent as a tool. Models:
  OpenRouter, or OpenCode Go with `RIG_MODEL=opencode-go/<model>`.
- README: the call example sets `default_max_turns`. Rig's default of one turn failed the
  first remote tool call.

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
