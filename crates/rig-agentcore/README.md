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
rig_agentcore::AgentCoreApp::new(agent)
    .record_content(true) // prompts and replies on the spans, for AgentCore Evaluations
    .serve()              // 0.0.0.0:$PORT, default 8080; stops on SIGTERM
    .await?;
```

- **Sessions:** the `X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header names the
  conversation; each session's messages are kept and sent with its next invocation.
- **Traces:** each invocation runs in an `agentcore.invocation` span that continues the
  caller's trace (`traceparent`) and gives every span under it `session.id`, through
  [`rig-otel`](../rig-otel). With rig-otel's `aws` feature exporting to CloudWatch, that
  is what AgentCore Observability and AgentCore Evaluations read.
- **Content:** `record_content(true)` records prompts and replies on the GenAI spans.
  AgentCore Evaluations needs them to judge a session; they are off by default, as in
  Rig.

## Testing

`cargo test` serves the contract over real HTTP for agents on scripted models: ping, a
session keeping its conversation, sessions kept apart, streamed replies, a bad body, and
an invocation continuing the caller's trace with the session on every span.
