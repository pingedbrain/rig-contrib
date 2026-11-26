# rig-a2a

[Agent2Agent (A2A) protocol](https://a2a-protocol.org/latest/specification/) v1.0 for
[Rig](https://github.com/0xPlaygrounds/rig) agents, both ways, built on the official
A2A Rust SDK ([`a2a-lf`](https://github.com/a2aproject/a2a-rs)):

- **Serve** a Rig agent to any A2A client: `A2aServer::new(agent, url).into_router()`
  gives an Axum router with the agent card, JSON-RPC and HTTP+JSON bindings, SSE
  streaming, task get/list/cancel and per-`contextId` conversation history.
- **Call** any A2A agent from a Rig agent: `RemoteAgent::connect(url).await?.into_tool()`
  gives a `DynamicTool` named after the remote agent's card. Answers keep their text,
  structured data and files; the model gets a JSON summary plus any image itself.
- **Follow** an answer as it is written: `remote.stream(text, None)` yields
  `Status`, `Text` (append), `Rewrite` (replace) and finally `Done(Reply)`.

```rust
// Serve
let app = rig_a2a::A2aServer::new(agent, "http://127.0.0.1:3000").into_router();
let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
rig_a2a::axum::serve(listener, app).await?;

// Call
let remote = rig_a2a::RemoteAgent::connect("http://127.0.0.1:3000").await?;
let agent = AgentBuilder::new(model).dynamic_tool(remote.into_tool()).build();
```

Targets Rig 0.47. Native only (no wasm).

## Interoperability: A2A v1.0 and v0.3

The root speaks **v1.0**. Peers still on **v0.3**, such as Strands Agents 1.x
(`a2a-sdk<0.4`), use the v0.3 surface under `/v03`, which translates to the same
handler, task store and conversation history:

| Path | What |
|---|---|
| `/.well-known/agent-card.json` | v1.0 card (public) |
| `/jsonrpc`, `/rest` | v1.0 JSON-RPC and HTTP+JSON |
| `/v03/.well-known/agent-card.json` | v0.3 card (public) |
| `/v03` | v0.3 JSON-RPC: `message/send`, `message/stream` (SSE), `tasks/get`, `tasks/cancel` |

`RemoteAgent::connect` reads the card and picks the version it declares, so the same
call reaches v1.0 and v0.3 agents.

Verified against Strands Agents 1.66 (Python) with real models, both ways:
`A2AAgent(endpoint="http://host:3000/v03")` calling a Rig agent, and a Rig agent calling
a Strands `A2AServer` as a tool.

## Replies: text, data, files

`Reply { text, data, files, context_id, state }`. As a tool result the model sees:

- one JSON block: `reply`, `context_id`, `state`, `data` (structured parts) and
  `files` (name, media type, URL or size; small text files inlined);
- then each image, as an image the model can look at (inline up to 5 MB, or by URL).

Numbers in data parts can come back as floats over v1.0 (`42` as `42.0`): the
protocol carries them as a protobuf `Struct`, whose numbers are doubles.

## Streaming

When the card declares streaming, `send` and the tool follow the answer as a stream,
and the timeout bounds the silence between events instead of the whole task, so a
long task that keeps reporting is not cut short. A stream that stops before the task
finishes (a refused call, a dropped connection) is an error, never an empty answer.
Both ways agents stream are understood: artifact chunks (Rig, most SDKs) and
`working` status messages followed by a final artifact (Strands).

## Serving: what comes back

A plain agent answers with text, streamed, plus any image the model produced as a
file part. `A2aServer::typed::<T>()` serves an agent whose callers want data: every task
runs as `prompt_typed::<T>` and answers with one JSON data part, and the card declares
`application/json` output and the JSON schema of `T`.

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
struct Task { title: String, priority: Priority }

A2aServer::new(extractor_agent, url).typed::<Task>().into_router();
```

## Conversations

Messages sharing a `contextId` share the agent's history. By default it lives in memory
(at most 1000 conversations, oldest forgotten first). `history_store` keeps it in any
[`rig-memory`](https://crates.io/crates/rig-memory) store (`DirStore` on disk,
`rig-sqlite`, `rig-postgres`), so it survives restarts and replicas share it; two
servers saving one conversation at once lose neither turn. Each request sends the model
only the newest messages that fit `history_window` (32k tokens by default), never a tool
result without its call. A `contextId` must be 1-128 of `[A-Za-z0-9_-]`, so a client
cannot make it escape a file store.

```rust
A2aServer::new(agent, url)
    .history_store(rig_memory::DirStore::new("./conversations"))
    .history_window(rig_memory::recent::TokenBudget::new(16_000));
```

## Production knobs

Every task spends model tokens, so an exposed agent is bounded by default:

| Knob | Default | Beyond it |
|---|---|---|
| `max_concurrent_tasks` | 32 | rejected as busy, before any model call |
| `max_run_duration` | 10 min | the run is stopped, the task fails |
| `max_request_bytes` | 2 MiB | HTTP 413 |
| `bearer_token` | none | HTTP 401 on every call (cards stay public) |

```rust
A2aServer::new(agent, url)
    .bearer_token(token)
    .max_concurrent_tasks(8)
    .max_run_duration(Duration::from_secs(120));

RemoteAgent::builder(url).bearer_token(token).timeout(Duration::from_secs(60)).connect().await?;
```

## Tracing

Each task runs in an `a2a.task` span (`a2a.task_id`, `a2a.context_id`, and `a2a.state`
recorded when it ends: `completed`, `failed`, `failed_timeout`, `rejected`,
`rejected_busy`). Rig's own spans for the model calls and tools nest under it, so an
OpenTelemetry backend shows each A2A task with everything it spent.

## Not supported

Push notifications (webhooks), the authenticated extended agent card, OAuth/OIDC
schemes (bearer only; add your own Axum middleware for more), the gRPC binding, and
files produced by tools (only the model's own images become file parts).

## Examples

| Example | What |
|---|---|
| `cargo run --example serve` | An OpenAI-backed agent served over A2A (`OPENAI_API_KEY`) |
| `cargo run --example echo_server` | A keyless, deterministic agent answering `pong` (for tests) |
| `cargo run --example call -- <url> <text>` | Call any A2A agent, v1.0 or v0.3, streaming the answer |

## Testing and CI

`cargo test` covers the server, the client, v0.3 translation, auth, timeouts,
streaming assembly and rich replies against scripted models: no network, no keys.

`interop/run.sh` checks real interoperability, keyless, against independent
implementations: Strands Agents 1.66 in both of its streaming modes (A2A v0.3),
both directions, and the official A2A Python SDK 1.x (A2A v1.0), with and without the
bearer token. CI runs both on every push, plus MSRV, `cargo-deny` and a weekly run
to catch new releases upstream.

## Credits

The server half (`server.rs`, `executor.rs`, `parts.rs`, `tests/serve.rs`) is adapted
from the unmerged `rig-a2a` crate in 0xPlaygrounds/rig#2820 (branch `triage/a2a`), by the
Rig maintainers, ported to the Rig 0.47 API. MIT licensed; see `LICENSE`.
