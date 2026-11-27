# rig-contrib

[![CI](https://github.com/pingedbrain/rig-contrib/actions/workflows/ci.yml/badge.svg)](https://github.com/pingedbrain/rig-contrib/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV 1.97.1](https://img.shields.io/badge/MSRV-1.97.1-orange.svg)](Cargo.toml)
[![Rig 0.47](https://img.shields.io/badge/Rig-0.47-8a2be2.svg)](https://github.com/0xPlaygrounds/rig)

Companion crates for [Rig](https://github.com/0xPlaygrounds/rig): agent-to-agent
protocols, multi-agent orchestration, observability, evals and deployment on AWS.
They are maintained outside Rig's repository, and their versions follow Rig's: 0.47.x
works with Rig 0.47.

## Crates

- **[`rig-a2a`](crates/rig-a2a)**: Serve a Rig agent over the A2A protocol (v1.0, and v0.3 for Strands Agents), and call remote A2A agents as Rig tools.  
  [crates.io](https://crates.io/crates/rig-a2a) · [docs](https://docs.rs/rig-a2a)
- **[`rig-mcp-server`](crates/rig-mcp-server)**: Serve Rig agents and tools over the Model Context Protocol, over stdio or Streamable HTTP.  
  [crates.io](https://crates.io/crates/rig-mcp-server) · [docs](https://docs.rs/rig-mcp-server)
- **[`rig-multiagent`](crates/rig-multiagent)**: Swarms of agents that hand work to each other, and graphs of agents and functions; streamed, with a person in the loop, resumable from checkpoints.  
  [crates.io](https://crates.io/crates/rig-multiagent) · [docs](https://docs.rs/rig-multiagent)
- **[`rig-otel`](crates/rig-otel)**: OpenTelemetry in one call: Rig's spans over OTLP, GenAI metrics, sessions and traces across processes, with SigV4 export to Amazon CloudWatch.  
  [crates.io](https://crates.io/crates/rig-otel) · [docs](https://docs.rs/rig-otel)
- **[`rig-evals`](crates/rig-evals)**: Evaluate Rig agents: datasets, built-in and LLM-judge evaluators, AWS AgentCore Evaluations, pass-rate thresholds for CI, JUnit/JSON/Markdown reports.  
  [crates.io](https://crates.io/crates/rig-evals) · [docs](https://docs.rs/rig-evals)
- **[`rig-agentcore`](crates/rig-agentcore)**: Run a Rig agent on AWS Bedrock AgentCore Runtime: its HTTP contract, sessions, streaming and the traces AgentCore reads.  
  [crates.io](https://crates.io/crates/rig-agentcore) · [docs](https://docs.rs/rig-agentcore)

Each crate stands on its own; add only the ones you need:

```sh
cargo add rig-a2a rig-otel
```

## How they fit together

```mermaid
flowchart LR
    agent["Your Rig agent"]
    agent --> a2a["rig-a2a<br/>A2A server and client"]
    agent --> mcp["rig-mcp-server<br/>MCP server"]
    agent --> multi["rig-multiagent<br/>swarms and graphs"]
    agent --> agentcore["rig-agentcore<br/>AgentCore Runtime"]
    evals["rig-evals"] -->|runs and scores| agent
    a2a -. traces .-> otel["rig-otel"]
    mcp -. traces .-> otel
    agentcore -. traces .-> otel
    otel --> backends["OTLP backends<br/>CloudWatch, Jaeger, Langfuse..."]
```

`rig-otel` is the shared piece. With the `otel` feature of `rig-a2a` and
`rig-mcp-server` (always on in `rig-agentcore`), a call from one agent to another
continues the caller's trace and session.

## Settings across crates

Each cell is the builder method on that crate, with its default in brackets. `—` means the crate has no such setting. Turn budgets and content recording for the agent itself come from Rig's `AgentBuilder` (`default_max_turns`, `record_content_telemetry`).

| Concept | rig-a2a | rig-mcp-server | rig-multiagent | rig-otel | rig-evals | rig-agentcore |
|---|---|---|---|---|---|---|
| Run timeout | `max_run_duration` (600 s) | `max_run_duration` (600 s) | `execution_timeout` (15 min) on `Swarm` and `Graph`; per step `agent_timeout` (`Swarm`) or `node_timeout` (`Graph`), 5 min | — | `trial_timeout` (off) | `max_run_duration` (no limit) |
| Concurrency cap | `max_concurrent_tasks` (32) | `max_concurrent_calls` (32) | — | — | `concurrency` (4 trials, not requests) | `max_concurrent_tasks` (no limit) |
| History window | `history_window` (32,000 tokens) | `history_window` (32,000 tokens) | — | — | — | `history_window` (32,000 tokens) |
| Max sessions | `max_contexts` (1,000) | `max_sessions` (1,000) | — | — | — | `max_sessions` (1,000) |
| Request size limit | `max_request_bytes` (2 MiB) | `max_request_bytes` (4 MiB, HTTP only) | — | — | — | `max_request_bytes` (2 MiB) |
| Content recording | — | — | — | — | — | `record_content` (false) |
| Bearer token | `bearer_token` on `A2aServer` (none); `bearer_token` on `RemoteAgentBuilder` (none) | `bearer_token` (none, HTTP only) | — | — | — | — |
| Host allowlist | `allowed_hosts` on `A2aServer` (none, HTTP 403 when set) | `allowed_hosts` (localhost, 127.0.0.1, ::1; HTTP only) | — | — | — | — |
| Turn budget | — | — | `max_turns_per_agent` (8) | — | — | — |

Session ids are spelled differently in each crate, so they are not a builder setting:

| Crate | Session id |
|---|---|
| rig-a2a | `contextId` in the A2A message |
| rig-mcp-server | `session_id` argument of agent tools |
| rig-multiagent | — |
| rig-otel | `session.id` span attribute, set with `set_session` |
| rig-evals | `session_id` on `TargetOutput::with_session_id`; AgentCore queries use `session.id` |
| rig-agentcore | `x-amzn-bedrock-agentcore-runtime-session-id` header (`SESSION_HEADER`) |

Doc aliases make the same concept findable under the other crate's name: `max_sessions` for `max_contexts` in rig-a2a, `max_concurrent_tasks` for `max_concurrent_calls` in rig-mcp-server, and `node_timeout` and `agent_timeout` on the two rig-multiagent builders.

## Verified

- **Interoperability:** CI runs `rig-a2a` against Strands Agents and the official A2A
  SDK in both directions, and `rig-mcp-server` against the official MCP Python SDK,
  over stdio and HTTP.
- **AWS:** `rig-agentcore`, `rig-otel` and `rig-evals` were run on AWS Bedrock
  AgentCore, with traces in CloudWatch and AgentCore Evaluations scoring the agent; see
  [rig-agentcore's README](crates/rig-agentcore#verified-on-aws).
- **Every push:** the tests of every crate, Clippy with Rig's workspace lints, the MSRV
  and `cargo-deny`; weekly, the newest compatible Rig.

The code follows Rig's own lints and conventions, so a crate here could move into
Rig's repository as it is.

## Issues

Each crate has a label (`rig-a2a`, `rig-otel`, ...). Open issues in this repository and
pick the crate's label, or name the crate in the title.

## License

MIT. `rig-a2a` adapts code from Rig (0xPlaygrounds/rig#2820) and carries its notice.
