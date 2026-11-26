# rig-contrib

Companion crates for [Rig](https://github.com/0xPlaygrounds/rig), maintained outside
Rig's repository. Their versions follow Rig's: 0.47.x works with Rig 0.47.

| Crate | What it does |
|---|---|
| [`rig-a2a`](crates/rig-a2a) | Serve a Rig agent over the A2A protocol (v1.0, and v0.3 for Strands Agents), and call remote A2A agents as Rig tools. |
| [`rig-mcp-server`](crates/rig-mcp-server) | Serve Rig agents and tools over the Model Context Protocol, over stdio or Streamable HTTP. |
| [`rig-otel`](crates/rig-otel) | OpenTelemetry in one call: Rig's spans over OTLP, GenAI metrics, sessions and traces across processes, in the shape AWS Bedrock AgentCore reads. |
| [`rig-multiagent`](crates/rig-multiagent) | Swarms of agents that hand work to each other, and graphs of agents and functions; streamed, with a person in the loop, resumable from checkpoints. |

Each crate's README covers its use; the CI runs every crate's tests, the MSRV,
`cargo-deny`, the interoperability suites against independent implementations (the
official A2A and MCP Python SDKs, Strands Agents), and, weekly, the newest compatible Rig.

The code follows Rig's own workspace lints, so a crate here could move into Rig's
repository as it is.

## License

MIT. `rig-a2a` adapts code from Rig (0xPlaygrounds/rig#2820) and carries its notice.
