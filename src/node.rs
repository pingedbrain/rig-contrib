//! What a graph runs: agents, swarms, graphs and plain async functions.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use rig_agent::Agent;
use rig_agent::AgentRunner;
use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_core::completion::GenerationOptions;
use rig_core::completion::Usage;

use crate::checkpoint::{Journal, Saving};
use crate::events::stream_agent;
use crate::human::{Question, answer};

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
/// Implemented for Rig agents, [`Swarm`](crate::Swarm)s, [`Graph`](crate::Graph)s,
/// [`human`](crate::human) and, through [`node_fn`], async functions.
pub trait Node: Send + Sync {
    /// Run the step on `input`. Through `io` it can report the text it
    /// writes and ask a person, when the graph run is
    /// [streamed](crate::Graph::stream).
    fn run<'a>(
        &'a self,
        input: String,
        io: &'a NodeIo<'a>,
    ) -> BoxFuture<'a, Result<NodeOutput, NodeError>>;
}

/// What a node can do while it runs: report text and ask a person (in a
/// streamed run), and keep its progress (in a checkpointed one).
pub struct NodeIo<'a> {
    node: &'a str,
    text: Option<&'a (dyn Fn(&str) + Send + Sync)>,
    question: Option<&'a (dyn Fn(Question) + Send + Sync)>,
    saving: Option<Saving>,
}

impl<'a> NodeIo<'a> {
    /// The view of node `node` in a plain run: its text goes nowhere, and
    /// it cannot ask. Build one to run a node yourself.
    pub fn quiet(node: &'a str) -> Self {
        Self {
            node,
            text: None,
            question: None,
            saving: None,
        }
    }

    /// The view of node `node` in a streamed run: its text goes to `text`,
    /// its questions to `question`.
    pub fn streaming(
        node: &'a str,
        text: &'a (dyn Fn(&str) + Send + Sync),
        question: &'a (dyn Fn(Question) + Send + Sync),
    ) -> Self {
        Self {
            node,
            text: Some(text),
            question: Some(question),
            saving: None,
        }
    }

    pub(crate) fn saving_to(mut self, saving: Option<Saving>) -> Self {
        self.saving = saving;
        self
    }

    /// The node's id.
    pub fn node(&self) -> &str {
        self.node
    }

    /// Whether the run is streamed: text is reported and questions asked.
    pub fn is_streaming(&self) -> bool {
        self.text.is_some()
    }

    /// Report text the node wrote; nothing in a plain run.
    pub fn text(&self, text: &str) {
        if let Some(sink) = self.text {
            sink(text);
        }
    }

    /// Ask a person `question` about `context`, and wait for the answer.
    /// Fails in a plain run, where nobody can answer.
    pub async fn ask(
        &self,
        question: impl Into<String>,
        context: impl Into<String>,
    ) -> Result<String, NodeError> {
        let Some(ask) = self.question else {
            return Err("this node asks a person: run the graph with stream()".into());
        };
        let (question, reply) = Question::new(self.node, question, context);
        ask(question);
        answer(reply).await
    }

    /// Pass on a question from inside the node (a nested swarm's agent).
    pub(crate) fn forward(&self, question: Question) {
        if let Some(ask) = self.question {
            ask(question.via(self.node));
        }
    }

    /// Where this run of the node keeps its progress, in a checkpointed run.
    pub(crate) fn saving(&self) -> Option<&Saving> {
        self.saving.as_ref()
    }
}

impl Node for Agent {
    /// In a checkpointed run, the agent's tool results go to a journal, so
    /// a resumed run replays them rather than executing the tools again.
    fn run<'a>(
        &'a self,
        input: String,
        io: &'a NodeIo<'a>,
    ) -> BoxFuture<'a, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let meter = Meter::default();
            let key = io.saving().map(|saving| saving.run_id.clone());
            let mut runner = in_conversation(self.prompt(input), self, key);
            runner = runner.add_hook(meter.clone());
            if let Some(saving) = io.saving() {
                runner = runner.add_hook(Journal::open(saving.kind("journal")).await?);
            }
            let response = if io.is_streaming() {
                stream_agent(runner, &|text: &str| io.text(text)).await?
            } else {
                runner.run().await?
            };
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
    fn run<'a>(
        &'a self,
        input: String,
        _io: &'a NodeIo<'a>,
    ) -> BoxFuture<'a, Result<NodeOutput, NodeError>> {
        let future = (self.0)(input);
        Box::pin(async move { Ok(NodeOutput::new(future.await?)) })
    }
}

/// Give one agent run its own prompt-cache key, `key` or a fresh one, unless
/// the agent sets its own. Gateways such as OpenCode Go route a
/// conversation's calls by it and reuse their prompt cache; without one Rig
/// sends a fresh id with every call, so no call of a run reuses another's.
pub(crate) fn in_conversation(
    runner: AgentRunner,
    agent: &Agent,
    key: Option<String>,
) -> AgentRunner {
    if agent.run_spec().options.cache_key.is_some() {
        return runner;
    }
    let key = key.unwrap_or_else(fresh_key);
    runner.options(GenerationOptions::default().cache_key(key))
}

/// A key no other run of any process shares.
fn fresh_key() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let run = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("rig-multiagent-{}-{started:x}-{run}", std::process::id())
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
