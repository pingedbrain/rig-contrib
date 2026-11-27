# rig-a2a

[Agent2Agent (A2A) protocol](https://a2a-protocol.org/latest/specification/) v1.0 for
[Rig](https://github.com/0xPlaygrounds/rig) agents, both ways, built on the official
A2A Rust SDK ([`a2a-lf`](https://github.com/a2aproject/a2a-rs)):

- **Serve** a Rig agent to any A2A client: `A2aServer::new(agent, url).into_router()`
  gives an Axum router with the agent card, JSON-RPC and HTTP+JSON bindings, SSE
  streaming, task get/list/cancel and per-`contextId` conversation history.
- **Call** any A2A agent from a Rig agent: `RemoteAgent::connect(url).await?.into_tool()?`
  gives a `DynamicTool` named after the remote agent's card. Answers keep their text,
  structured data and files; the model gets a JSON summary plus any image itself.
- **Follow** an answer as it is written: `remote.stream(text, None)` yields
  `Status`, `Text` (append), `Rewrite` (replace) and finally `Done(Reply)`.

```rust
use rig_a2a::A2aServer;
use rig_agent::Agent;

const URL: &str = "http://127.0.0.1:3000";

/// Serve an agent over A2A: the agent card and the protocol endpoints.
async fn serve(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let app = A2aServer::new(agent, URL).into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

```rust
use rig_a2a::RemoteAgent;

const URL: &str = "http://127.0.0.1:3000";

/// Ask a remote A2A agent directly. Its answer is `reply.text`.
async fn ask(question: &str) -> Result<String, Box<dyn std::error::Error>> {
    let remote = RemoteAgent::connect(URL).await?;
    let reply = remote.send(question, None).await?;
    Ok(reply.text)
}
```

```rust
use rig_a2a::RemoteAgent;
use rig_agent::AgentBuilder;

const URL: &str = "http://127.0.0.1:3000";

/// Give a model a remote A2A agent as a tool. Rig's default is one turn, so the
/// first tool call would fail: allow the call and the answer.
async fn call_remote(builder: AgentBuilder) -> Result<rig_agent::Agent, Box<dyn std::error::Error>> {
    let remote = RemoteAgent::connect(URL).await?;
    Ok(builder.dynamic_tool(remote.into_tool()?).default_max_turns(4).build())
}
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
- then each image, as an image the model can look at (inline up to 5 MiB, or by URL).

Numbers in data parts can come back as floats over v1.0 (`42` as `42.0`): the
protocol carries them as a protobuf `Struct`, whose numbers are doubles.

## Streaming

When the card declares streaming, `send` and the tool follow the answer as a stream,
and the timeout bounds the silence between events instead of the whole task, so a
long task that keeps reporting is not cut short. A stream that stops before the task
finishes (a refused call, a dropped connection) is an error, never an empty answer.
Both ways agents stream are understood: artifact chunks (Rig, most SDKs) and
`working` status messages followed by a final artifact (Strands).

A second client can attach to a running task with `tasks/resubscribe`
(`A2AClient::subscribe_to_task`). It gets the task's updates from then on, and the
stream ends when the task does.

Limits on what a remote agent may send, enforced by the client:

- agent card: 1 MiB. A larger card is a `RemoteError::Card` at `connect`.
- v0.3 `message/send` reply: 16 MiB. A larger reply is an error.
- v0.3 stream: one event is at most 16 MiB before its blank line. A longer event ends
  the stream with an error.
- image in a tool result: 5 MiB (see Replies).

## Serving: what comes back

A plain agent answers with text, streamed, plus any image the model produced as a
file part. `A2aServer::typed::<T>()` serves an agent whose callers want data: every task
runs as `prompt_typed::<T>` and answers with one JSON data part, and the card declares
`application/json` output and the JSON schema of `T`.

An agent with tools needs `AgentBuilder::default_max_turns(n)` with `n >= 2`. Without it the run stops after its first model call, and the failed task says so.

```rust
use rig_a2a::A2aServer;
use rig_agent::Agent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct Task { title: String, priority: Priority }

#[derive(Serialize, Deserialize, JsonSchema)]
enum Priority { Low, High }

fn extractor(extractor_agent: Agent, url: &str) -> axum::Router {
    A2aServer::new(extractor_agent, url).typed::<Task>().into_router()
}
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
use rig_a2a::A2aServer;
use rig_agent::Agent;
use rig_memory::{DirStore, recent::TokenBudget};

fn conversations(agent: Agent, url: &str) -> Result<axum::Router, rig_memory::SessionError> {
    let store = DirStore::open("./conversations")?;
    Ok(A2aServer::new(agent, url)
        .history_store(store)
        .history_window(TokenBudget::new(16_000))
        .into_router())
}
```

## Production knobs

Every task spends model tokens, so an exposed agent is bounded by default:

| Knob | Default | Beyond it |
|---|---|---|
| `max_concurrent_tasks` | 32 | rejected as busy, before any model call |
| `max_run_duration` | 10 min | the run is stopped, the task fails |
| `max_request_bytes` | 2 MiB | HTTP 413 |
| `bearer_token` | none | HTTP 401 on every call (cards stay public) |
| `allowed_hosts` | none | HTTP 403 when the `Host` header is not listed, cards included |

A server with no bearer token answers any `Host` header. A web page on another site can use DNS
rebinding to reach a local server this way, so set `allowed_hosts` to the names clients use
(`localhost`, your public host name). An entry without a port matches any port; `host:port`
matches only that port. Unset, the `Host` header is not checked.

Finished tasks are a different case: the default store keeps them all, so memory grows with
every task. Set `terminal_task_limit` to purge the oldest finished tasks. A purged task
answers `GetTask` with `TaskNotFound`. Running tasks are never purged. It is off by default
because a client may fetch a finished task long after it ended.

```rust
use std::time::Duration;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::Agent;

async fn bounded(agent: Agent, url: &str, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    let _app = A2aServer::new(agent, url)
        .bearer_token(token)
        .max_concurrent_tasks(8)
        .max_run_duration(Duration::from_secs(120))
        .terminal_task_limit(1_000)
        .into_router();

    let _remote = RemoteAgent::builder(url)
        .bearer_token(token)
        .timeout(Duration::from_secs(60))
        .connect()
        .await?;
    Ok(())
}
```

## Push notifications

Off by default: the card says `pushNotifications: false`, and a config set is refused
with `PushNotificationNotSupported`. Turn them on with `push_notifications(true)`, and
set a `bearer_token` too: without one, push stays off and the server logs an error. A
client then sets a webhook for a task, on the send or with `tasks/pushNotificationConfig/set`,
and the server POSTs each task update to it.

```rust
use rig_a2a::A2aServer;
use rig_agent::Agent;

/// Serve an agent with push notifications, for webhooks on one allowlisted host.
fn serve_with_push(agent: Agent, token: &str) -> A2aServer {
    A2aServer::new(agent, "https://agent.example.com")
        .bearer_token(token)
        .push_notifications(true)
        .push_allowed_hosts(["hooks.example.com"])
        .terminal_task_limit(1_000)
}
```

**Risks.** A webhook URL comes from the client, so it is a server-side request forgery
(SSRF) risk, and the webhook receives task content. Read these before turning push on:

- **Webhook URLs are client input.** With no allowlist, any host the address guard allows
  can receive POSTs. Set `push_allowed_hosts` when clients are not trusted.
- **Receivers must authenticate.** The server signs nothing (no JWT or JWKS). It sends the
  client's `token` as `A2A-Notification-Token` and the client's `authentication`
  credentials as `Authorization`. A receiver must check both itself.
- **Updates carry full content.** Each POST has the whole task update, including the
  agent's output and artifacts.
- **Task ids are secrets.** Anyone who holds a task id can attach a webhook to it and get
  its updates. That is why push needs the bearer token; the token is the only check on
  who may attach one.
- **Slow webhooks slow tasks.** Delivery runs in the task's event loop, one config at a
  time, with a 5 second timeout per POST. A task with 10 stalled configs can take 50
  seconds per update, and the client's stream waits for it.
- **Configs outlive tasks.** A task holds up to 10 configs, and the server up to 10,000
  counted since it started. A finished task keeps its configs until the total is reached;
  then the configs of tasks purged by `terminal_task_limit` are dropped, so set it as above.

Saving a config checks its URL: it must be `https` (`http` only with
`allow_unguarded_push_urls()`), on the allowlist if one is set, and not a literal IP in
100.64.0.0/10, 64:ff9b::/96, 2002::/16 or ::a.b.c.d/96.

When a webhook is called, the SDK refuses these, including names that resolve to them and
redirects to them: 127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16,
224.0.0.0/4, 0.0.0.0, ::1, ::, fc00::/7, fe80::/10, ff00::/8, their IPv4-mapped forms, and
the hosts `localhost`, `metadata.google.internal` and `metadata.azure.com`. Names that
resolve into the literal-check ranges above are not refused at delivery.

`allow_unguarded_push_urls()` drops the `https` requirement, the save-time refusal of the
literal ranges above, and the SDK's address guard, and lets the SDK use the proxies named
in the environment. Use it for tests against a
webhook on this machine only. `push_config_store` keeps configs in your own store, and
the same checks and caps apply to it. The v0.3 card under `/v03` keeps
`pushNotifications: false`.

## Tracing

Each task runs in an `a2a.task` span (`a2a.task_id`, `a2a.context_id`, and `a2a.state`
recorded when it ends: `completed`, `failed`, `failed_timeout`, `rejected`,
`rejected_busy`). Rig's own spans for the model calls and tools nest under it, so an
OpenTelemetry backend shows each A2A task with everything it spent. Each `RemoteAgent`
call runs in an `a2a.call` span (`a2a.agent`).

### Tracing across agents

With the `otel` feature, a Rig agent calling a remote Rig agent makes **one trace**
across both processes, via `rig-otel`:

- every call (v1.0 and v0.3) carries the W3C `traceparent` and `baggage` of its
  `a2a.call` span;
- the server's `a2a.task` span continues that trace and starts a session: its
  `session.id`, on it and every span under it, is the
  `X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header when AWS Bedrock AgentCore
  Runtime sends one, else the task's A2A `contextId`.

```toml
rig-a2a = { version = "0.47", features = ["otel"] }
```

Install an OpenTelemetry subscriber on both sides (`rig_otel::init()`); without the
feature nothing changes and no OpenTelemetry crates are pulled in.

## Content recording

Prompts and replies go on the agent's spans only when it records content. It is off by
default, as in Rig. Set it on the agent you serve with
`AgentBuilder::record_content_telemetry(true)`.

## Not supported

The authenticated extended agent card, OAuth/OIDC
schemes (bearer only; add your own Axum middleware for more), the gRPC binding, and
files produced by tools (only the model's own images become file parts).

## Examples

| Example | What |
|---|---|
| `cargo run --example a2a_serve` | An OpenAI-backed agent served over A2A (`OPENAI_API_KEY`) |
| `cargo run --example a2a_echo_server` | A keyless, deterministic agent answering `pong` (for tests) |
| `cargo run --example call -- <url> <text>` | Call any A2A agent, v1.0 or v0.3, streaming the answer |
| `cargo run --example remote_tool` | A local Rig agent that calls a remote A2A agent as a tool. Keyless by default (scripted models); `RIG_MODEL` or `OPENROUTER_MODEL` for a live run |

## Testing and CI

`cargo test` covers the server, the client, v0.3 translation, auth, timeouts,
streaming assembly and rich replies against scripted models: no network, no keys.

`interop/run.sh` checks real interoperability, keyless, against independent
implementations: Strands Agents 1.66 in both of its streaming modes (A2A v0.3),
both directions, and the official A2A Python SDK 1.x (A2A v1.0), with and without the
bearer token. CI runs both on every push, plus MSRV, `cargo-deny` and a weekly run
to catch new releases upstream.

## Credits

The server half (`server.rs`, `executor.rs`, `parts.rs`, `tests/integration/serve.rs`) is adapted
from the unmerged `rig-a2a` crate in 0xPlaygrounds/rig#2820 (branch `triage/a2a`), by the
Rig maintainers, ported to the Rig 0.47 API. MIT licensed; see `LICENSE`.
