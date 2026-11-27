# Changelog

Versions follow Rig's: rig-agentcore 0.47.x works with Rig 0.47.

## Unreleased

- Fix: `AgentCoreApp` no longer turns off content recording that the agent turned on. Without `AgentCoreApp::record_content`, the agent's `record_content_telemetry(true)` now applies to its spans; before, the app passed `false` on every invocation. `record_content(true)` or `record_content(false)` still overrides the agent. The invocation span's prompt and answer fields follow `record_content` only, so they need `record_content(true)`.
- Docs: content recording is documented the same way as in the other crates.
- `serve()` logs a `warn` when `PORT` is set but is not a port number, and serves on 8080 as
  before, so the app stays reachable. The warning names the value.
- Docs: content recording is documented the same way as in the other crates, and `AgentCoreApp::record_content` overrides the agent's own setting.
- README: "Verified on AWS" is rewritten to stand alone. It lists the verified deployment and gives the public steps (ARM64 image, ECR, execution role, runtime, invocation, Transaction Search) with the AWS CLI. The image is built from `deploy/Dockerfile`, which the crate does not publish.
- `AgentCoreApp::spawn_task(future)` runs background work on Tokio, and `AgentCoreApp::track_task()`
  returns a `BusyGuard` for work the app runs itself. While either is open, `/ping` answers
  `HealthyBusy`, so AgentCore Runtime keeps the session running. The status returns to `Healthy`
  when the work ends, including when its task is aborted.
- `/ping` adds `time_of_last_update` (Unix seconds), which moves only when the status changes, as
  AgentCore's HTTP contract asks. An idle app still answers `Healthy` with the same `status` field.
- README snippets are compile-checked as doctests, so the examples cannot rot.
- `AgentCoreApp::summarize_history(summarizer, keep_recent, trigger_messages)`: opt-in. Past
  `trigger_messages`, a session's older messages become one summary, its first message: a user
  message in rig-memory's compaction frame, made by `summarizer` (an agent without tools). The
  newest `keep_recent` stay verbatim, and a tool call is never split from its result. The
  summary is always sent first, even when `history_window` is narrower. `trigger_messages` is
  raised to `keep_recent + 1` when not larger. A failed summary (error, empty reply, or a store
  change under it) is logged and the history is kept; concurrent invocations may both summarize.
  Without a store, a session whose summaries keep failing keeps at least its newest
  `2 * trigger_messages` messages, or what the window keeps if that is more. Built on
  rig-memory's compaction helpers, as rig-memory has no summarizing window. In a session
  this option summarizes, a first message that opens with the frame text is taken as the summary.
- `AgentCoreApp::max_concurrent_tasks(n)`: opt-in cap on invocations running at once. Past
  it, an invocation gets 429 Too Many Requests before the agent runs. No cap by default.
- `AgentCoreApp::max_run_duration(d)`: opt-in limit on one invocation, summarizing included.
  Past it, the run is stopped and answers 504 Gateway Timeout (or an `{"error"}` event when
  streaming), and its turn is not added to the session. No limit by default.
- `AgentCoreApp::max_request_bytes(n)`, with `DEFAULT_MAX_REQUEST_BYTES` (2 MB): an
  `/invocations` body over the limit gets 413 Payload Too Large with the JSON error, where it
  got 400 before. The default keeps the limit already in place.

## 0.47.3 (2026-11-27)

- **Behavior change:** each session keeps only the newest messages that fit
  `history_window` (32,000 tokens by default). Older turns beyond that are no longer
  kept or sent to the model; before, a session grew and sent all of its history.
- `AgentCoreApp::history_window`, as in rig-a2a and rig-mcp-server.
- `AgentCoreApp::history_store`, as in rig-a2a and rig-mcp-server: keeps sessions in any
  rig-memory store (`DirStore` on disk, `rig-sqlite`, `rig-postgres`), so they survive a
  restart. The store keeps each session's whole conversation; `history_window` limits only
  what is sent. A session id that is not 1 to 128 ASCII letters, digits, `-` or `_` is
  hashed into its log name, so any AgentCore session id works. A save that loses a race
  retries up to 10 times; if it still fails, the answer is returned, the failure is logged,
  and that turn is not persisted.
- `rig_agentcore::rig_memory` re-exports rig-memory, so the API follows rig-memory 0.47.x.
- `examples/serve.rs`: serves the contract locally on `PORT` (default 8080), on a scripted
  model or on OpenRouter when `OPENROUTER_MODEL` and `OPENROUTER_API_KEY` are set.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.
- Links between the crates' READMEs point to crates.io, so they work there.

## 0.47.0 (2026-11-26)

First release.

- `AgentCoreApp`: AgentCore Runtime's HTTP contract for a Rig agent, `/invocations` in
  JSON or server-sent events and `/ping`; a conversation per runtime session; spans that
  continue the caller's trace and carry `session.id`; optional content recording for
  AgentCore Evaluations.
