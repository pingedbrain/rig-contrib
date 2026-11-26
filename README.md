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

| Crate | | What it does |
|---|---|---|
| [`rig-a2a`](crates/rig-a2a) | [![crates.io](https://img.shields.io/crates/v/rig-a2a.svg)](https://crates.io/crates/rig-a2a) [![docs.rs](https://img.shields.io/docsrs/rig-a2a)](https://docs.rs/rig-a2a) | Serve a Rig agent over the A2A protocol (v1.0, and v0.3 for Strands Agents), and call remote A2A agents as Rig tools. |
| [`rig-mcp-server`](crates/rig-mcp-server) | [![crates.io](https://img.shields.io/crates/v/rig-mcp-server.svg)](https://crates.io/crates/rig-mcp-server) [![docs.rs](https://img.shields.io/docsrs/rig-mcp-server)](https://docs.rs/rig-mcp-server) | Serve Rig agents and tools over the Model Context Protocol, over stdio or Streamable HTTP. |
| [`rig-multiagent`](crates/rig-multiagent) | [![crates.io](https://img.shields.io/crates/v/rig-multiagent.svg)](https://crates.io/crates/rig-multiagent) [![docs.rs](https://img.shields.io/docsrs/rig-multiagent)](https://docs.rs/rig-multiagent) | Swarms of agents that hand work to each other, and graphs of agents and functions; streamed, with a person in the loop, resumable from checkpoints. |
| [`rig-otel`](crates/rig-otel) | [![crates.io](https://img.shields.io/crates/v/rig-otel.svg)](https://crates.io/crates/rig-otel) [![docs.rs](https://img.shields.io/docsrs/rig-otel)](https://docs.rs/rig-otel) | OpenTelemetry in one call: Rig's spans over OTLP, GenAI metrics, sessions and traces across processes, with SigV4 export to Amazon CloudWatch. |
| [`rig-evals`](crates/rig-evals) | [![crates.io](https://img.shields.io/crates/v/rig-evals.svg)](https://crates.io/crates/rig-evals) [![docs.rs](https://img.shields.io/docsrs/rig-evals)](https://docs.rs/rig-evals) | Evaluate Rig agents: datasets, built-in and LLM-judge evaluators, AWS AgentCore Evaluations, pass-rate thresholds for CI, JUnit/JSON/Markdown reports. |
| [`rig-agentcore`](crates/rig-agentcore) | [![crates.io](https://img.shields.io/crates/v/rig-agentcore.svg)](https://crates.io/crates/rig-agentcore) [![docs.rs](https://img.shields.io/docsrs/rig-agentcore)](https://docs.rs/rig-agentcore) | Run a Rig agent on AWS Bedrock AgentCore Runtime: its HTTP contract, sessions, streaming and the traces AgentCore reads. |

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
