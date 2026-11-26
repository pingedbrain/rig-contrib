# Changelog

Versions follow Rig's: rig-multiagent 0.47.x works with Rig 0.47.

## 0.47.0 (unreleased)

First release.

- `Swarm`: named Rig agents that hand work to each other through a `handoff_to_agent`
  tool, with handoff messages and shared context, limits on handoffs and time, and
  repetition detection. A handoff ends the agent's turn.
- `Graph`: agents, swarms, graphs and async functions (`node_fn`) as nodes; edges with
  optional conditions; parallel batches; inputs built from dependencies; bounded
  cycles; failed branches pruned while the rest finish.
- Token usage of every model call, stopped turns included; tracing spans per run and
  per turn or node.
