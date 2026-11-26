//! What a graph runs: agents, swarms, graphs and plain async functions.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use rig_agent::Agent;
use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_core::completion::Usage;

/// Why a node failed.
pub type NodeError = Box<dyn std::error::Error + Send + Sync>;

/// What a node produced.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NodeOutput {
    /// The text handed to the nodes that follow.
    pub text: String,
    /// The model tokens the node spent.
    pub usage: Usage,
}

impl NodeOutput {
    /// An output with no model usage.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            usage: Usage::default(),
        }
    }

    /// Record the model tokens spent producing it.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

/// One step of a [`Graph`](crate::Graph): takes the text built from the task
/// and the outputs before it, and produces text.
///
/// Implemented for Rig agents, [`Swarm`](crate::Swarm)s, [`Graph`](crate::Graph)s
/// and, through [`node_fn`], async functions.
pub trait Node: Send + Sync {
    /// Run the step on `input`.
    fn run(&self, input: String) -> BoxFuture<'_, Result<NodeOutput, NodeError>>;
}

impl Node for Agent {
    fn run(&self, input: String) -> BoxFuture<'_, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let meter = Meter::default();
            let response = self.prompt(input).add_hook(meter.clone()).run().await?;
            Ok(NodeOutput::new(response.output()).with_usage(meter.total()))
        })
    }
}

/// A node made of an async function; see [`node_fn`].
pub struct FnNode<F>(F);

/// A node that runs `f`: deterministic steps between agents (parsing,
/// validation, a lookup, a tool) without a model call.
///
/// ```
/// use rig_multiagent::node_fn;
/// let shout = node_fn(|input: String| async move { Ok(input.to_uppercase()) });
/// ```
pub fn node_fn<F, Fut>(f: F) -> FnNode<F>
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: Future<Output = Result<String, NodeError>> + Send + 'static,
{
    FnNode(f)
}

impl<F, Fut> Node for FnNode<F>
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: Future<Output = Result<String, NodeError>> + Send + 'static,
{
    fn run(&self, input: String) -> BoxFuture<'_, Result<NodeOutput, NodeError>> {
        let future = (self.0)(input);
        Box::pin(async move { Ok(NodeOutput::new(future.await?)) })
    }
}

/// Adds up the usage of every completion an agent run makes, including the
/// ones the run retried or a hook stopped, which `PromptResponse::usage`
/// misses when the run ends early.
#[derive(Clone, Default)]
pub(crate) struct Meter(Arc<Mutex<Usage>>);

impl Meter {
    pub(crate) fn total(&self) -> Usage {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AgentHook for Meter {
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(completion) = event.completion() {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) += completion.usage;
        }
        OutcomeAction::proceed()
    }
}
