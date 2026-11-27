//! Multi-agent patterns for [Rig](https://github.com/0xPlaygrounds/rig).
//!
//! - A [`Swarm`] is a team of agents that pass the work to each other: each
//!   agent gets a `handoff_to_agent` tool, and the first agent that answers
//!   without handing off gives the swarm's answer.
//! - A [`Graph`] runs [nodes](Node) (agents, swarms, graphs or plain async
//!   functions) along edges, some of them conditional. Nodes that become ready
//!   together run in parallel, and each node gets the outputs of the nodes
//!   that led to it.
//!
//! ```no_run
//! use rig_multiagent::{Graph, Swarm};
//! # async fn run(
//! #     researcher: rig_agent::Agent,
//! #     coder: rig_agent::Agent,
//! #     reviewer: rig_agent::Agent,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! // The researcher starts and hands off to the coder when it is done.
//! let swarm = Swarm::builder().agent(researcher).agent(coder).build()?;
//! let run = swarm.run("Add a retry to the HTTP client").await?;
//! println!("{} answered: {}", run.agent, run.answer);
//!
//! // The same team as one step of a graph, followed by a review.
//! let graph = Graph::builder()
//!     .node("build", swarm)
//!     .node("review", reviewer)
//!     .edge("build", "review")
//!     .build()?;
//! let run = graph.run("Add a retry to the HTTP client").await?;
//! println!("{}", run.answer());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::unreachable
    )
)]

mod checkpoint;
mod events;
mod graph;
mod human;
mod node;
mod swarm;

pub use checkpoint::CheckpointError;
pub use graph::{Graph, GraphBuilder, GraphError, GraphEvent, GraphFailure, GraphRun, GraphState};
pub use human::{HumanNode, Question, human};
pub use node::{FnNode, Node, NodeError, NodeIo, NodeOutput, node_fn};
/// The stores runs keep their checkpoints in.
pub use rig_memory;
pub use swarm::{
    ASK_TOOL, HANDOFF_TOOL, Handoff, Swarm, SwarmBuilder, SwarmError, SwarmEvent, SwarmFailure,
    SwarmRun,
};

/// Why a [`Swarm`] or [`Graph`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// Nothing to run.
    #[error("a swarm or graph needs at least one member")]
    Empty,
    /// A swarm agent without a name: the other agents hand off to it by name.
    #[error("every swarm agent needs a name (AgentBuilder::name)")]
    UnnamedAgent,
    /// Two members share a name.
    #[error("two members are named {0:?}")]
    DuplicateName(String),
    /// An entry point or edge names a member that does not exist.
    #[error("no member is named {0:?}")]
    UnknownName(String),
    /// Every graph node has an incoming edge, so none is a natural start.
    #[error("every node has an incoming edge; name where the graph starts with entry()")]
    NoEntry,
    /// A graph with a cycle and no bound on how many times nodes may run.
    #[error("the graph has a cycle; bound it with max_node_executions()")]
    UnboundedCycle,
    /// A turn budget of zero: an agent could not make its first model call.
    #[error("max_turns_per_agent must be at least 1")]
    ZeroTurns,
}

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme_doctests {}
