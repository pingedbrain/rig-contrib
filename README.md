# rig-multiagent

Multi-agent patterns for [Rig](https://github.com/0xPlaygrounds/rig): a **Swarm** of
agents that hand work to each other, and a **Graph** of agents, swarms, graphs and plain
functions with conditional edges and parallel branches.

Rig composes agents by making one agent a tool of another. This crate adds the two
patterns where no single agent is in charge.

```rust
use rig_multiagent::{Graph, GraphState, Swarm};

// Agents that pass the work on: each gets a `handoff_to_agent` tool.
let team = Swarm::builder()
    .agent(explorer)       // named Rig agents; descriptions tell the others what each is for
    .agent(writer)
    .max_handoffs(6)
    .build()?;
let run = team.run("What does this project do?").await?;
println!("{} answered: {}", run.agent, run.answer);

// Steps with dependencies: strengths and risks run in parallel, report joins them.
let review = Graph::builder()
    .node("explore", explorer)
    .node("strengths", strengths)
    .node("risks", risks)
    .node("report", report)
    .edge("explore", "strengths")
    .edge("explore", "risks")
    .edge("strengths", "report")
    .edge("risks", "report")
    .build()?;
println!("{}", review.run("Review src/tools.rs").await?.answer());
```

## Swarm

The run starts with the entry agent (the first one added, or `entry(name)`). Each turn,
the agent sees the user request, the handoff message it was given, who worked on the
task before, what they shared, and the other agents with their descriptions. It either
answers, and that answer is the swarm's, or calls `handoff_to_agent(agent_name, message,
context?)`. A handoff ends the agent's turn right there (a Rig hook stops the run), so it
does not keep working on what the next agent now owns, nor spend tokens on it.

Every turn starts fresh; what one agent tells the next travels in the handoff message
and the shared context, which every later agent sees. A handoff to an unknown agent, or
to itself, is a tool error that names the valid agents, so the model can correct itself.

| Limit | Default | Beyond it |
|---|---|---|
| `max_handoffs` | 20 | `SwarmFailure::MaxHandoffs` |
| `agent_timeout` | 5 min | `SwarmFailure::Timeout` |
| `execution_timeout` | 15 min | `SwarmFailure::Timeout` |
| `detect_repetition(window, min_unique)` | off | `SwarmFailure::Repetitive`: the last `window` turns went to fewer than `min_unique` agents |

A `SwarmRun` has the answer, the agent that gave it, every turn and handoff in order,
the token usage of every model call (stopped turns included) and the elapsed time. A
`SwarmError` carries the same trail up to the failure.

The swarm adds `handoff_to_agent` to each agent's tool set, which clones of the agent
share.

## Graph

A run starts with the entry nodes (by default, those without incoming edges), batch by
batch: the nodes of a batch run in parallel; each node that finished follows its
outgoing edges whose condition holds (`edge_if(from, to, |state| ...)`), and the nodes
they reach make the next batch, each once. A node gets the task if nothing led to it, or
else the task plus the outputs of the finished nodes with an edge to it that holds:

```text
Original task: Review src/tools.rs

Inputs from previous nodes:

From strengths:
...

From risks:
...
```

A node is anything implementing `Node`: a Rig `Agent`, a `Swarm`, another `Graph`, or an
async function through `node_fn`, for deterministic steps between agents (parsing,
validation, a lookup) that need no model.

Cycles are allowed, for loops like a reviewer sending a draft back to its writer until
it approves; a node that runs again sees the newest outputs. A graph with a cycle must
set `max_node_executions`, or it does not build.

A failed node does not follow its edges, so its branch stops while the others carry on;
the run then returns a `GraphError` with every failure and everything the rest produced.

| Limit | Default | Beyond it |
|---|---|---|
| `max_node_executions` | none (required with cycles) | `GraphFailure::MaxNodeExecutions` |
| `node_timeout` | 5 min | `GraphFailure::NodeTimeout` |
| `execution_timeout` | 15 min | `GraphFailure::Timeout` |

`GraphRun::answer()` is the output of the last node to finish (or of each node in the
last batch, under its name); `output(id)` reads any node's.

## Streaming

`stream(task)` runs a swarm or graph while reporting what happens, so a UI or CLI shows
progress instead of waiting a minute for the answer:

```rust
use futures::StreamExt;
use rig_multiagent::SwarmEvent;

let mut events = team.stream("What does this project do?");
while let Some(event) = events.next().await {
    match event {
        SwarmEvent::AgentStarted { agent } => println!("\n[{agent}]"),
        SwarmEvent::Text { text, .. } => print!("{text}"),        // as the model writes it
        SwarmEvent::Handoff(handoff) => println!("\n-> {}", handoff.to),
        SwarmEvent::Finished(outcome) => println!("\n{:?}", (*outcome).map(|run| run.answer)),
        _ => {}
    }
}
```

| `SwarmEvent` | `GraphEvent` |
|---|---|
| `AgentStarted { agent }` | `NodeStarted { node }` |
| `Text { agent, text }` | `Text { node, text }` |
| `Handoff(Handoff)` | `NodeFinished { node, output }`, `NodeFailed { node, error }` |
| `Finished(Box<Result<SwarmRun, SwarmError>>)`, last | `Finished(Box<Result<GraphRun, GraphError>>)`, last |

Agents run in Rig's streaming mode, so their text arrives as the model writes it; a
swarm or graph used as a graph node streams its agents' text under that node. A custom
`Node` streams by implementing `run_streaming`; by default it reports only its output.
The run advances only while the stream is polled, and dropping the stream stops it.

## Tracing

Runs are `swarm.run` / `graph.run` spans, with a `swarm.agent` or `graph.node` span per
turn; Rig's own spans for model calls and tools nest under them.

## Compared with Strands Agents

Modeled on the [Strands](https://strandsagents.com) `Swarm` and `Graph`, with the same
semantics where they overlap: the handoff tool and the context it builds, the limits,
the repetition check, batch execution, conditional edges, input from dependencies,
pruning of failed branches, and nesting. Differences: a handoff ends the agent's turn
at once; a cyclic graph needs an execution bound; edge conditions are plain closures
over the run state. Not here (yet): interrupts (human-in-the-loop) and resuming a run
from a snapshot.

## Testing

`cargo test` runs swarms and graphs on Rig's scripted mock models and plain functions,
with no keys or network: handoffs, shared context, refused handoffs, every limit,
parallel branches (timed), joins, conditional edges, review loops, failures, timeouts,
nesting and usage accounting; streamed runs on scripted streaming models, with event
order, nested streaming and cancellation by dropping the stream.
