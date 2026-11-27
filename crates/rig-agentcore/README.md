# rig-agentcore

Run a [Rig](https://github.com/0xPlaygrounds/rig) agent on
[AWS Bedrock AgentCore Runtime](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-http-protocol-contract.html).

AgentCore runs an agent as an ARM64 container serving its HTTP protocol contract on port
8080. `AgentCoreApp` serves it for a Rig agent:

| Path | What it does |
|---|---|
| `POST /invocations` | Takes `{"prompt": "..."}`. Answers `{"response": "...", "status": "success"}`, or server-sent events (`{"text"}` as the model writes, then `{"response"}`) when the caller accepts `text/event-stream`. |
| `GET /ping` | `{"status": "Healthy"}`. |

```rust
use rig_agent::Agent;
use rig_agentcore::AgentCoreApp;

async fn serve(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    AgentCoreApp::new(agent)
        .record_content(true) // prompts and replies on the spans, for AgentCore Evaluations
        .serve()              // 0.0.0.0:$PORT, default 8080; stops on SIGTERM
        .await?;
    Ok(())
}
```

- **Sessions:** the `X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header names the
  conversation; each session's messages are kept and sent with its next invocation.
  By default they live in memory (at most 1000 sessions, oldest forgotten first).
  `history_store` keeps them in any [`rig-memory`](https://crates.io/crates/rig-memory)
  store (`DirStore` on disk, `rig-sqlite`, `rig-postgres`), so they survive a restart
  and several apps can share them. Any session id works: one of 1-128 of `[A-Za-z0-9_-]`
  names its log as it is, any other is hashed into a safe log name. A save that loses
  a race with another writer retries up to 10 times; if it still fails, the answer is
  returned, the failure is logged, and that turn is not persisted. A failed load is an
  error reported before the agent runs.
  Each invocation sends the newest messages that fit `history_window` (32k tokens by
  default, `rig_memory::recent::TokenBudget`), never splitting a tool call from its
  result. An empty window sends no history. In memory, messages beyond the window are
  forgotten when a run is saved. With `history_store`, the store keeps the whole
  conversation and the window only limits what is sent.

  ```rust
  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn sessions(agent: Agent) -> Result<AgentCoreApp, rig_memory::SessionError> {
      Ok(AgentCoreApp::new(agent)
          .history_store(rig_memory::DirStore::open("/var/lib/agent/sessions")?)
          .history_window(rig_memory::recent::TokenBudget::new(16_000)))
  }
  ```

  `DirStore` needs rig-memory's `fs` feature: enable it in your own `Cargo.toml`
  (`rig-memory = { version = "0.47", features = ["fs"] }`).
- **Summaries:** `summarize_history(summarizer, keep_recent, trigger_messages)` makes a
  long session shorter. Once a session holds more than `trigger_messages` messages, an
  agent without tools (`summarizer`) summarizes all but the newest `keep_recent`, and that
  one summary becomes the session's first message. The cut never separates a tool call
  from its result. If the summary fails, a warning is logged, the session keeps its full
  history, and the invocation answers as usual. With `history_store`, the summary is written
  to the store in place of the messages it replaces. Rig-memory has no summarizing window
  (its compaction is a planning API the caller drives), so this option is built on its
  compaction helpers.

  ```rust
  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn app(agent: Agent, summarizer: Agent) -> AgentCoreApp {
      AgentCoreApp::new(agent).summarize_history(summarizer, 20, 60)
  }
  ```
- **Traces:** each invocation runs in an `agentcore.invocation` span that continues the
  caller's trace (`traceparent`) and gives every span under it `session.id`, through
  [`rig-otel`](https://crates.io/crates/rig-otel). With rig-otel's `aws` feature exporting to CloudWatch, that
  is what AgentCore Observability and AgentCore Evaluations read.
- **Limits:** all opt-in. `max_concurrent_tasks` caps invocations running at once (past
  the cap, 429 before the agent runs; no cap by default, since AgentCore Runtime gives each
  session its own microVM). `max_run_duration` stops an invocation, summarizing included,
  after a time (504, or an `{"error"}` event when streaming, and its turn is not kept; no
  limit by default). `max_request_bytes` sets the body limit (default 2 MB, answered with 413).
  Outside AgentCore Runtime, a cap around 32 and a ten-minute run limit suit most agents.

  ```rust
  use std::time::Duration;

  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn limited(agent: Agent) -> AgentCoreApp {
      AgentCoreApp::new(agent)
          .max_concurrent_tasks(32)
          .max_run_duration(Duration::from_secs(600))
          .max_request_bytes(256 * 1024)
  }
  ```
- **Content:** `record_content(true)` records prompts and replies on the GenAI spans.
  AgentCore Evaluations needs them to judge a session; they are off by default, as in
  Rig.

## Verified on AWS

On 2026-11-26, rig-lab (a Rig agent with a calculator tool, Claude Haiku 4.5 on
Bedrock) ran on AgentCore Runtime in us-east-1 with `record_content(true)` and
rig-otel's `aws` export, deployed as AWS documents it (ECR image, execution role,
Transaction Search). A two-turn session (3×47−29, then ÷4) kept its conversation, its
spans reached CloudWatch (`aws/spans`) under the session id, and AgentCore Evaluations'
built-in evaluators judged it from those spans: GoalSuccessRate 1.0, Helpfulness 0.83
and Correctness 1.0 per turn. The scripts are in rig-lab's `deploy/agentcore`.

What made the spans evaluable: the scope `opentelemetry.instrumentation.rig`,
`session.id` on every span, an `invoke_agent` span with `gen_ai.task.input` and
`gen_ai.task.output`, and Rig's `chat` and `execute_tool` spans with content recorded.

## Testing

`cargo test` serves the contract over real HTTP for agents on scripted models: ping, a
session keeping its conversation, sessions kept apart, streamed replies, a bad body,
the run and body limits, and an invocation continuing the caller's trace with the session on every span.
