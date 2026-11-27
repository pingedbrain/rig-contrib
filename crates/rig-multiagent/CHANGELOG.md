# Changelog

Versions follow Rig's: rig-multiagent 0.47.x works with Rig 0.47.

## Unreleased

- Fix: the agent a handoff reaches is now given what the agents before it wrote and read
  in their turns (text and tool results), not only the handoff message and context. Before,
  the next agent saw only that message, so the work a handed-back agent had done was lost
  to it. The prompt now says an agent that can complete the request should answer it, and
  hands off only when another agent must act. No API change; checkpoints from before still
  load.
- `examples/swarm_graph.rs`: `RIG_MODEL=opencode-go/<model>` runs the live snippets on OpenCode
  Go (`OPENCODE_GO_API_KEY`); live replies may use 1024 tokens; a failed swarm run prints its
  handoffs.
- `Swarm::detect_repetition` is documented and tested: two agents passing the work back
  and forth fail with `SwarmFailure::Repetitive` once the last window of turns went to
  fewer agents than the minimum. Still off by default; a run with no window is never
  stopped for repetition, and a forward chain (A, B, C) is not stopped.
- The handoff tool's description now asks the model to answer directly when it can, and
  not to hand back to the agent that just handed off unless it has new information.
  Prompt text only; the tool's name and parameters are unchanged.
- `examples/swarm_graph.rs`: the Swarm, streaming and Graph snippets, on scripted models
  with no key; `OPENROUTER_MODEL` (with `OPENROUTER_API_KEY`) runs them live.
- `Swarm::max_turns_per_agent(n)` and `Graph::max_turns_per_agent(n)`: the model calls one
  agent run may make, eight by default; zero fails the build with `BuildError::ZeroTurns`.
  An agent's own `default_max_turns` is kept when it is higher.
- Behavior change, no API break: with Rig's budget of one, a run that made a tool call
  other than a handoff ran the tool and then failed with `MaxTurns`. It now continues to
  its answer. A handoff or a direct answer takes one model call under either budget, so
  those runs make the same calls as before. Set `max_turns_per_agent(1)` to restore the
  old limit.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.

## 0.47.0 (2026-11-26)

First release.

- `Swarm`: named Rig agents that hand work to each other through a `handoff_to_agent`
  tool, with handoff messages and shared context, limits on handoffs and time, and
  repetition detection. A handoff ends the agent's turn.
- `Graph`: agents, swarms, graphs and async functions (`node_fn`) as nodes; edges with
  optional conditions; parallel batches; inputs built from dependencies; bounded
  cycles; failed branches pruned while the rest finish.
- `Swarm::stream` and `Graph::stream`: turns or nodes starting and finishing, the text
  agents write as they write it, handoffs, and the outcome last; nested swarms and graphs
  stream their text; dropping the stream stops the run.
- A person in the loop: `Question` events in streamed runs; `human(question)` graph
  nodes; `SwarmBuilder::allow_questions` and the `ask_human` tool; `NodeIo::ask` for
  custom nodes. Time waiting for an answer does not count against timeouts.
- Checkpoints in any rig-memory store (`checkpoints`): `run_as` / `stream_as` resume a
  run after a failure or a restart, keeping its progress, its pending questions and its
  counted time; `Resumed` events. Graphs save as each node finishes; every agent run
  keeps a journal of its tool results (and answers from people), replayed instead of
  run again; nested swarms and graphs resume inside their node. A checkpoint that cannot
  be read or written fails the run with a typed `CheckpointError`.
- `Node::run(input, io)` with `NodeIo` (text, questions, saving) replaces the separate
  streaming method.
- One prompt-cache key per conversation (swarm turn, agent node), stable across
  resumes, unless the agent sets its own.
- Token usage of every model call, stopped turns included; tracing spans per run and
  per turn or node.
