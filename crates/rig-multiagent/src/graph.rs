//! Nodes run along edges, in parallel where they can.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rig_core::completion::Usage;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmBoxedStream, WasmCompatSend, WasmCompatSync};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::BuildError;
use crate::checkpoint::{CheckpointError, Checkpoints, Saving};
use crate::events::{Emit, events};
use crate::human::{Clock, Question};
use crate::node::{Node, NodeError, NodeIo, NodeOutput};
use crate::swarm::DEFAULT_MAX_TURNS;

/// An edge's condition.
#[cfg(not(target_family = "wasm"))]
type Condition = Arc<dyn Fn(&GraphState) -> bool + Send + Sync>;
/// An edge's condition (browser wasm: no `Send + Sync`, no threads).
#[cfg(target_family = "wasm")]
type Condition = Arc<dyn Fn(&GraphState) -> bool>;

/// Nodes (agents, swarms, graphs, functions) connected by edges.
///
/// A run starts with the [entry](GraphBuilder::entry) nodes, by default the
/// ones without incoming edges, on the task. Then, batch by batch: the nodes
/// of a batch run in parallel; a node that finished follows each of its
/// outgoing edges whose condition holds, and the nodes those edges reach make
/// the next batch. A node reached from several nodes of a batch runs once.
/// A node gets the task when nothing led to it, and otherwise the task with
/// the outputs of every finished node with an edge to it that holds.
///
/// A node that fails does not follow its edges, so its branch stops; the
/// other branches carry on, and the run then fails with what it got.
///
/// With cycles, a node can run again (a reviewer sending work back to a
/// writer, say), seeing the newest outputs before it; such a graph needs
/// [`max_node_executions`](GraphBuilder::max_node_executions).
///
/// Cloning is cheap; clones share the nodes.
#[derive(Clone)]
pub struct Graph {
    inner: Arc<Inner>,
}

struct Inner {
    nodes: Vec<GraphNode>,
    edges: Vec<Edge>,
    entries: Vec<usize>,
    max_node_executions: Option<usize>,
    max_turns: usize,
    execution_timeout: Duration,
    node_timeout: Duration,
    checkpoints: Option<Checkpoints>,
}

struct GraphNode {
    id: String,
    node: Box<dyn Node>,
}

struct Edge {
    from: usize,
    to: usize,
    condition: Option<Condition>,
}

impl Edge {
    fn holds(&self, state: &GraphState) -> bool {
        self.condition
            .as_ref()
            .is_none_or(|condition| condition(state))
    }
}

/// Builds a [`Graph`].
#[must_use]
pub struct GraphBuilder {
    nodes: Vec<(String, Box<dyn Node>)>,
    edges: Vec<(String, String, Option<Condition>)>,
    entries: Vec<String>,
    max_node_executions: Option<usize>,
    max_turns: usize,
    execution_timeout: Duration,
    node_timeout: Duration,
    checkpoints: Option<Checkpoints>,
}

impl Graph {
    /// Start building a graph.
    pub fn builder() -> GraphBuilder {
        GraphBuilder {
            nodes: Vec::new(),
            edges: Vec::new(),
            entries: Vec::new(),
            max_node_executions: None,
            max_turns: DEFAULT_MAX_TURNS,
            execution_timeout: Duration::from_secs(15 * 60),
            node_timeout: Duration::from_secs(5 * 60),
            checkpoints: None,
        }
    }
}

impl GraphBuilder {
    /// Add a node: a Rig agent, a [`Swarm`](crate::Swarm), another
    /// [`Graph`], a [`node_fn`](crate::node_fn), or any [`Node`].
    pub fn node(mut self, id: impl Into<String>, node: impl Node + 'static) -> Self {
        self.nodes.push((id.into(), Box::new(node)));
        self
    }

    /// Run `to` after `from`.
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.push((from.into(), to.into(), None));
        self
    }

    /// Run `to` after `from` when `condition` holds on the run so far.
    ///
    /// ```
    /// # use rig_multiagent::{Graph, GraphState, node_fn};
    /// # let review = node_fn(|_: String| async { Ok("APPROVED".to_owned()) });
    /// # let publish = node_fn(|text: String| async { Ok(text) });
    /// let graph = Graph::builder()
    ///     .node("review", review)
    ///     .node("publish", publish)
    ///     .edge_if("review", "publish", |state: &GraphState| {
    ///         state.output("review").is_some_and(|verdict| verdict.contains("APPROVED"))
    ///     })
    ///     .build()?;
    /// # Ok::<(), rig_multiagent::BuildError>(())
    /// ```
    pub fn edge_if(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&GraphState) -> bool + WasmCompatSend + WasmCompatSync + 'static,
    ) -> Self {
        self.edges
            .push((from.into(), to.into(), Some(Arc::new(condition))));
        self
    }

    /// Start the run with this node (call it again for several). Defaults to
    /// every node without incoming edges.
    pub fn entry(mut self, id: impl Into<String>) -> Self {
        self.entries.push(id.into());
        self
    }

    /// How many node runs, in all, a graph run may make. Required when the
    /// graph has a cycle; unbounded otherwise.
    pub fn max_node_executions(mut self, max: usize) -> Self {
        self.max_node_executions = Some(max);
        self
    }

    /// How many model calls one agent node's run may make, counting the first
    /// call and every follow-up after a tool call. An agent whose own
    /// `default_max_turns` is higher keeps its own. Defaults to 8; zero fails
    /// the build with [`BuildError::ZeroTurns`]. Nodes that are swarms or
    /// functions are not affected.
    pub fn max_turns_per_agent(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// How long a whole run may take. Defaults to 15 minutes.
    pub fn execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    /// How long one node may take. Defaults to 5 minutes.
    pub fn node_timeout(mut self, timeout: Duration) -> Self {
        self.node_timeout = timeout;
        self
    }

    /// Keep the progress of runs started with [`run_as`](Graph::run_as) or
    /// [`stream_as`](Graph::stream_as) in `store` (any rig-memory store: a
    /// `DirStore`, a database), so a run that stopped resumes where it was.
    pub fn checkpoints(
        mut self,
        store: impl rig_memory::AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
    ) -> Self {
        self.checkpoints = Some(Checkpoints::new(store, "graph"));
        self
    }

    /// Check the nodes and edges.
    pub fn build(self) -> Result<Graph, BuildError> {
        if self.nodes.is_empty() {
            return Err(BuildError::Empty);
        }
        if self.max_turns == 0 {
            return Err(BuildError::ZeroTurns);
        }
        let mut nodes: Vec<GraphNode> = Vec::with_capacity(self.nodes.len());
        for (id, node) in self.nodes {
            if nodes.iter().any(|other| other.id == id) {
                return Err(BuildError::DuplicateName(id));
            }
            nodes.push(GraphNode { id, node });
        }
        let index = |id: &str| {
            nodes
                .iter()
                .position(|node| node.id == id)
                .ok_or_else(|| BuildError::UnknownName(id.to_owned()))
        };
        let mut edges = Vec::with_capacity(self.edges.len());
        for (from, to, condition) in self.edges {
            edges.push(Edge {
                from: index(&from)?,
                to: index(&to)?,
                condition,
            });
        }
        let entries = if self.entries.is_empty() {
            let starts: Vec<usize> = (0..nodes.len())
                .filter(|&node| !edges.iter().any(|edge| edge.to == node))
                .collect();
            if starts.is_empty() {
                return Err(BuildError::NoEntry);
            }
            starts
        } else {
            let mut entries = Vec::new();
            for id in &self.entries {
                let entry = index(id)?;
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            entries
        };
        if self.max_node_executions.is_none() && has_cycle(nodes.len(), &edges) {
            return Err(BuildError::UnboundedCycle);
        }
        Ok(Graph {
            inner: Arc::new(Inner {
                nodes,
                edges,
                entries,
                max_node_executions: self.max_node_executions,
                max_turns: self.max_turns,
                execution_timeout: self.execution_timeout,
                node_timeout: self.node_timeout,
                checkpoints: self.checkpoints,
            }),
        })
    }
}

/// Whether following edges (conditions aside) can come back to a node.
fn has_cycle(nodes: usize, edges: &[Edge]) -> bool {
    // Kahn's algorithm: a graph is acyclic when every node can be peeled off
    // once nothing points to it any more.
    let mut incoming = vec![0usize; nodes];
    for edge in edges {
        if let Some(count) = incoming.get_mut(edge.to) {
            *count += 1;
        }
    }
    let mut free: Vec<usize> = incoming
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| node)
        .collect();
    let mut peeled = 0;
    while let Some(node) = free.pop() {
        peeled += 1;
        for edge in edges.iter().filter(|edge| edge.from == node) {
            if let Some(count) = incoming.get_mut(edge.to) {
                *count -= 1;
                if *count == 0 {
                    free.push(edge.to);
                }
            }
        }
    }
    peeled < nodes
}

/// A run so far, as edge conditions see it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphState {
    task: String,
    outputs: HashMap<String, String>,
    runs: HashMap<String, usize>,
    /// How many times each node finished, which numbers its next run.
    #[serde(default)]
    finished: HashMap<String, usize>,
    order: Vec<String>,
}

impl GraphState {
    /// The task the run started with.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The newest output of node `id`, once it has finished.
    pub fn output(&self, id: &str) -> Option<&str> {
        self.outputs.get(id).map(String::as_str)
    }

    /// How many times node `id` has run, failures included.
    pub fn runs(&self, id: &str) -> usize {
        self.runs.get(id).copied().unwrap_or(0)
    }

    /// The nodes that have run, in order, failures included.
    pub fn order(&self) -> &[String] {
        &self.order
    }
}

/// A graph run, whole or (in a [`GraphError`]) partial.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GraphRun {
    /// What the run did.
    pub state: GraphState,
    /// The nodes of the last batch that finished, in order.
    pub last: Vec<String>,
    /// The model tokens the nodes that finished spent.
    pub usage: Usage,
    /// How long the run took.
    pub elapsed: Duration,
}

impl GraphRun {
    /// The newest output of node `id`.
    pub fn output(&self, id: &str) -> Option<&str> {
        self.state.output(id)
    }

    /// The run's answer: the output of the last node to finish, or, when
    /// the last batch had several, each one under its node's name.
    pub fn answer(&self) -> String {
        match self.last.as_slice() {
            [only] => self.output(only).unwrap_or_default().to_owned(),
            last => {
                let mut text = String::new();
                for id in last {
                    let _ = write!(
                        text,
                        "From {id}:\n{}\n\n",
                        self.output(id).unwrap_or_default()
                    );
                }
                text.trim_end().to_owned()
            }
        }
    }
}

/// A graph run in which something failed, with what the rest produced.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub struct GraphError {
    /// What failed, in order.
    pub failures: Vec<GraphFailure>,
    /// What the run did.
    pub run: GraphRun,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failures.as_slice() {
            [] => f.write_str("the graph run failed"),
            [only] => only.fmt(f),
            [first, rest @ ..] => write!(f, "{first} (and {} more failures)", rest.len()),
        }
    }
}

/// What a [streamed](Graph::stream) graph run reports, as it happens.
#[derive(Debug)]
#[non_exhaustive]
pub enum GraphEvent {
    /// The run resumed from its checkpoint: the first event of a resumed
    /// [`stream_as`](Graph::stream_as) run.
    Resumed {
        /// The nodes that had run, in order.
        done: Vec<String>,
    },
    /// A node started.
    NodeStarted {
        /// The node.
        node: String,
    },
    /// Text a node wrote: what an agent's model streams, or a nested swarm's
    /// or graph's. Nodes with nothing to stream report only their output.
    Text {
        /// The node.
        node: String,
        /// The new text.
        text: String,
    },
    /// A node finished.
    NodeFinished {
        /// The node.
        node: String,
        /// Its output.
        output: String,
    },
    /// A node failed or ran out of time; its branch stops.
    NodeFailed {
        /// The node.
        node: String,
        /// Why.
        error: String,
    },
    /// A node asks a person; the node waits for the [answer](Question::answer),
    /// and the run's timeouts stop meanwhile.
    Question(Question),
    /// The run ended: the last event. Boxed, so the many text events stay small.
    Finished(Box<Result<GraphRun, GraphError>>),
}

/// A run's checkpoint: what it did, and the batch that comes next.
#[derive(Serialize, Deserialize)]
struct Saved {
    finished: bool,
    state: GraphState,
    usage: Usage,
    last: Vec<String>,
    executions: usize,
    /// The nodes of the batch still to run, or the next batch.
    ready: Vec<String>,
    /// The nodes of the batch that had finished.
    #[serde(default)]
    batch_done: Vec<String>,
    /// Nodes that failed, to run again.
    failed: Vec<String>,
    active_ms: u64,
    elapsed_ms: u64,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Something that went wrong in a graph run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GraphFailure {
    /// A node failed.
    #[error("node {node:?} failed: {source}")]
    Node {
        /// The node.
        node: String,
        /// Its error.
        #[source]
        source: NodeError,
    },
    /// A node ran out of time.
    #[error("node {node:?} did not finish in time")]
    NodeTimeout {
        /// The node.
        node: String,
    },
    /// The checkpoint could not be read or written.
    #[error("checkpoint: {error}")]
    Checkpoint {
        /// What went wrong.
        #[source]
        error: CheckpointError,
    },
    /// The run used up [`GraphBuilder::execution_timeout`] before its next batch.
    #[error("the graph ran out of time")]
    Timeout,
    /// The next batch would go past [`GraphBuilder::max_node_executions`].
    #[error("the graph reached its limit of {limit} node runs")]
    MaxNodeExecutions {
        /// The limit.
        limit: usize,
    },
}

/// The span a graph run is in. Its totals are recorded when the run ends:
/// tokens under `graph.usage.*` (not `gen_ai.usage.*`, which the model calls
/// under it carry, so backends do not count them twice).
fn run_span(run_id: Option<&str>) -> tracing::Span {
    tracing::info_span!(
        "graph.run",
        graph.run_id = run_id,
        graph.node_runs = tracing::field::Empty,
        graph.usage.input_tokens = tracing::field::Empty,
        graph.usage.output_tokens = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    )
}

/// Record how a run ended on its span.
fn record_outcome(span: &tracing::Span, outcome: &Result<GraphRun, GraphError>) {
    let run = match outcome {
        Ok(run) => run,
        Err(error) => {
            span.record("otel.status_code", "ERROR");
            if let Some(failure) = error.failures.first() {
                span.record("error.type", failure.kind());
            }
            &error.run
        }
    };
    span.record("graph.node_runs", run.state.order().len());
    if let Some(tokens) = run.usage.input_tokens {
        span.record("graph.usage.input_tokens", tokens);
    }
    if let Some(tokens) = run.usage.output_tokens {
        span.record("graph.usage.output_tokens", tokens);
    }
}

/// Mark a node's span failed when it failed or ran out of time.
fn record_node<T>(span: &tracing::Span, outcome: &Option<Result<T, NodeError>>) {
    let kind = match outcome {
        Some(Ok(_)) => return,
        Some(Err(_)) => "node_failed",
        None => "timeout",
    };
    span.record("otel.status_code", "ERROR");
    span.record("error.type", kind);
}

impl GraphFailure {
    /// A short, stable name for the failure, for `error.type`.
    fn kind(&self) -> &'static str {
        match self {
            Self::Node { .. } => "node_failed",
            Self::NodeTimeout { .. } => "node_timeout",
            Self::Timeout => "timeout",
            Self::MaxNodeExecutions { .. } => "max_node_executions",
            Self::Checkpoint { .. } => "checkpoint",
        }
    }
}

impl Graph {
    /// Run the graph on `task`.
    pub async fn run(&self, task: impl Into<String>) -> Result<GraphRun, GraphError> {
        let span = run_span(None);
        let outcome = self
            .run_task(task.into(), None, false, None)
            .instrument(span.clone())
            .await;
        record_outcome(&span, &outcome);
        outcome
    }

    /// Run the graph as run `run_id`: resume it from its last checkpoint if
    /// it has one and did not finish, or else start it on `task`. Progress
    /// is saved as each node finishes and at the end, so a run that stopped
    /// runs only the nodes that had not finished, and a run that ended with
    /// failed nodes runs those nodes again.
    ///
    /// Agent nodes keep a journal of their tool results, which a node run
    /// again replays rather than executing the tools twice; swarm and graph
    /// nodes keep their own progress and resume inside too.
    ///
    /// Needs [`checkpoints`](GraphBuilder::checkpoints); without them it is
    /// [`run`](Self::run). A `run_id` is 1-128 of `[A-Za-z0-9_-]`.
    pub async fn run_as(
        &self,
        run_id: &str,
        task: impl Into<String>,
    ) -> Result<GraphRun, GraphError> {
        let span = run_span(Some(run_id));
        let outcome = self
            .run_task(task.into(), self.saving(Some(run_id)), false, None)
            .instrument(span.clone())
            .await;
        record_outcome(&span, &outcome);
        outcome
    }

    /// [`stream`](Self::stream) as run `run_id`, resuming it as
    /// [`run_as`](Self::run_as) does. A resumed run starts with
    /// [`GraphEvent::Resumed`].
    pub fn stream_as<'a>(
        &'a self,
        run_id: &'a str,
        task: impl Into<String>,
    ) -> WasmBoxedStream<'a, GraphEvent> {
        self.stream_events(task.into(), Some(run_id))
    }

    /// Run the graph on `task`, reporting each node as it starts, writes and
    /// finishes, and the outcome last.
    ///
    /// The run makes progress only while the stream is polled, and stops
    /// when the stream is dropped.
    pub fn stream(&self, task: impl Into<String>) -> WasmBoxedStream<'_, GraphEvent> {
        self.stream_events(task.into(), None)
    }

    fn stream_events<'a>(
        &'a self,
        task: String,
        run_id: Option<&'a str>,
    ) -> WasmBoxedStream<'a, GraphEvent> {
        let span = run_span(run_id);
        events(
            move |sender| async move {
                let emit = move |event| {
                    let _ = sender.send(event);
                };
                let outcome = self
                    .run_task(task, self.saving(run_id), false, Some(&emit))
                    .instrument(span.clone())
                    .await;
                record_outcome(&span, &outcome);
                outcome
            },
            |outcome| GraphEvent::Finished(Box::new(outcome)),
        )
    }

    /// Where run `run_id` keeps its progress, if the graph has a store.
    fn saving(&self, run_id: Option<&str>) -> Option<Saving> {
        run_id
            .zip(self.inner.checkpoints.clone())
            .map(|(id, checkpoints)| Saving::new(checkpoints, id))
    }

    /// Run `task`, saving to `saving`. A `nested` run (a graph node) that
    /// finished before answers as it did, rather than starting over: its
    /// graph lost track of it.
    async fn run_task(
        &self,
        task: String,
        saving: Option<Saving>,
        nested: bool,
        emit: Option<Emit<'_, GraphEvent>>,
    ) -> Result<GraphRun, GraphError> {
        let inner = &*self.inner;
        let started = Instant::now();
        let checkpoints = saving
            .as_ref()
            .map(|saving| (saving.run_id.as_str(), &saving.checkpoints));
        let fail = |error: CheckpointError, state: GraphState, before: Duration| GraphError {
            failures: vec![GraphFailure::Checkpoint { error }],
            run: GraphRun {
                state,
                last: Vec::new(),
                usage: Usage::default(),
                elapsed: before + started.elapsed(),
            },
        };
        let index_of = |id: &str| inner.nodes.iter().position(|node| node.id == id);
        let saved = match checkpoints {
            Some((id, checkpoints)) => match checkpoints.load::<Saved>(id).await {
                Ok(Some(saved)) if saved.finished && nested => {
                    return Ok(GraphRun {
                        state: saved.state,
                        last: saved.last,
                        usage: saved.usage,
                        elapsed: Duration::from_millis(saved.elapsed_ms),
                    });
                }
                Ok(saved) => saved.filter(|saved| !saved.finished),
                Err(error) => return Err(fail(error, GraphState::default(), Duration::ZERO)),
            },
            None => None,
        };
        let mut state;
        let mut usage;
        let mut last;
        let mut executions;
        let mut ready = Vec::new();
        // Nodes of the batch the run stopped in that had finished.
        let mut carried = Vec::new();
        let clock;
        let before;
        match saved {
            None => {
                state = GraphState {
                    task,
                    ..GraphState::default()
                };
                usage = Usage::default();
                last = Vec::new();
                executions = 0;
                ready = inner.entries.clone();
                clock = Clock::resumed(Duration::ZERO);
                before = Duration::ZERO;
            }
            Some(saved) => {
                before = Duration::from_millis(saved.elapsed_ms);
                // The nodes it had not run yet, and those that failed, run.
                for (ids, into) in [
                    (
                        saved.ready.iter().chain(&saved.failed).collect::<Vec<_>>(),
                        &mut ready,
                    ),
                    (saved.batch_done.iter().collect(), &mut carried),
                ] {
                    for id in ids {
                        let Some(index) = index_of(id) else {
                            let error = CheckpointError::UnknownNode(id.clone());
                            return Err(fail(error, saved.state, before));
                        };
                        if !into.contains(&index) {
                            into.push(index);
                        }
                    }
                }
                tracing::info!(done = saved.state.order.len(), "graph run resumed");
                if let Some(emit) = emit {
                    emit(GraphEvent::Resumed {
                        done: saved.state.order.clone(),
                    });
                }
                clock = Clock::resumed(Duration::from_millis(saved.active_ms));
                state = saved.state;
                usage = saved.usage;
                last = saved.last;
                executions = saved.executions;
            }
        }
        let clock = &clock;
        let mut failures = Vec::new();
        let names = |indices: &[usize]| -> Vec<String> {
            indices
                .iter()
                .filter_map(|&index| inner.nodes.get(index))
                .map(|node| node.id.clone())
                .collect()
        };
        let snapshot = |state: &GraphState,
                        usage: &Usage,
                        last: &[String],
                        executions: usize,
                        ready: &[usize],
                        batch_done: &[usize],
                        failures: &[GraphFailure]| Saved {
            finished: ready.is_empty() && batch_done.is_empty() && failures.is_empty(),
            state: state.clone(),
            usage: *usage,
            last: last.to_vec(),
            executions,
            ready: names(ready),
            batch_done: names(batch_done),
            failed: failures
                .iter()
                .filter_map(|failure| match failure {
                    GraphFailure::Node { node, .. } | GraphFailure::NodeTimeout { node } => {
                        Some(node.clone())
                    }
                    _ => None,
                })
                .collect(),
            active_ms: millis(clock.active()),
            elapsed_ms: millis(before + started.elapsed()),
        };
        'run: while !ready.is_empty() || !carried.is_empty() {
            let mut finished = std::mem::take(&mut carried);
            if !ready.is_empty() {
                if let Some((id, checkpoints)) = checkpoints {
                    let saved = snapshot(
                        &state, &usage, &last, executions, &ready, &finished, &failures,
                    );
                    if let Err(error) = checkpoints.save(id, &saved).await {
                        failures.push(GraphFailure::Checkpoint { error });
                        break 'run;
                    }
                }
                if let Some(limit) = inner.max_node_executions
                    && executions + ready.len() > limit
                {
                    failures.push(GraphFailure::MaxNodeExecutions { limit });
                    break 'run;
                }
                let remaining = inner.execution_timeout.saturating_sub(clock.active());
                if remaining.is_zero() {
                    failures.push(GraphFailure::Timeout);
                    break 'run;
                }
                let budget = inner.node_timeout.min(remaining);
                executions += ready.len();
                let mut batch: FuturesUnordered<_> = ready
                    .iter()
                    .filter_map(|&index| Some((index, inner.nodes.get(index)?)))
                    .map(|(index, node)| {
                        let input = self.input(index, &state);
                        // The node's n-th successful run: a failed run that
                        // runs again keeps the same part, journal included.
                        let part = state.finished.get(&node.id).copied().unwrap_or(0) + 1;
                        let saving = saving
                            .as_ref()
                            .map(|saving| saving.part("node", &node.id, part));
                        let span = tracing::info_span!(
                            "graph.node",
                            graph.node = %node.id,
                            otel.status_code = tracing::field::Empty,
                            error.type = tracing::field::Empty,
                        );
                        async move {
                            let Some(emit) = emit else {
                                let io = NodeIo::quiet(&node.id)
                                    .saving_to(saving)
                                    .with_turns(inner.max_turns);
                                let run = node.node.run(input, &io).instrument(span.clone());
                                let outcome = clock.within(budget, run).await;
                                record_node(&span, &outcome);
                                return (index, node, outcome);
                            };
                            emit(GraphEvent::NodeStarted {
                                node: node.id.clone(),
                            });
                            let on_text = |text: &str| {
                                emit(GraphEvent::Text {
                                    node: node.id.clone(),
                                    text: text.to_owned(),
                                })
                            };
                            let on_question = |question: Question| {
                                // The text stays out of telemetry, as Rig keeps
                                // prompts out unless content recording is on.
                                tracing::info!(graph.node = %node.id, "human.question");
                                emit(GraphEvent::Question(question.pausing(clock)))
                            };
                            let io = NodeIo::streaming(&node.id, &on_text, &on_question)
                                .saving_to(saving)
                                .with_turns(inner.max_turns);
                            let run = node.node.run(input, &io).instrument(span.clone());
                            let outcome = clock.within(budget, run).await;
                            record_node(&span, &outcome);
                            let id = node.id.clone();
                            emit(match &outcome {
                                Some(Ok(output)) => GraphEvent::NodeFinished {
                                    node: id,
                                    output: output.text.clone(),
                                },
                                Some(Err(error)) => GraphEvent::NodeFailed {
                                    node: id,
                                    error: error.to_string(),
                                },
                                None => GraphEvent::NodeFailed {
                                    node: id,
                                    error: "did not finish in time".to_owned(),
                                },
                            });
                            (index, node, outcome)
                        }
                    })
                    .collect();
                let mut remaining = ready.clone();
                while let Some((index, node, outcome)) = batch.next().await {
                    remaining.retain(|&other| other != index);
                    let id = node.id.clone();
                    *state.runs.entry(id.clone()).or_default() += 1;
                    state.order.push(id.clone());
                    match outcome {
                        Some(Ok(output)) => {
                            let NodeOutput {
                                text, usage: spent, ..
                            } = output;
                            usage += spent;
                            *state.finished.entry(id.clone()).or_default() += 1;
                            state.outputs.insert(id, text);
                            finished.push(index);
                        }
                        Some(Err(source)) => {
                            tracing::warn!(graph.node = %id, error = %source, "graph node failed");
                            failures.push(GraphFailure::Node { node: id, source });
                        }
                        None => failures.push(GraphFailure::NodeTimeout { node: id }),
                    }
                    // Saved as each node ends, so a stop mid-batch runs only
                    // the nodes still running. The batch's end is saved by
                    // the next batch's start, or the run's end.
                    if !remaining.is_empty()
                        && let Some((id, checkpoints)) = checkpoints
                    {
                        let counted = executions - remaining.len();
                        let saved = snapshot(
                            &state, &usage, &last, counted, &remaining, &finished, &failures,
                        );
                        if let Err(error) = checkpoints.save(id, &saved).await {
                            tracing::warn!(%error, "could not save a finished graph node");
                        }
                    }
                }
            }
            if !finished.is_empty() {
                last = names(&finished);
            }
            ready = Vec::new();
            for edge in &inner.edges {
                if finished.contains(&edge.from) && !ready.contains(&edge.to) && edge.holds(&state)
                {
                    ready.push(edge.to);
                }
            }
        }
        if let Some((id, checkpoints)) = checkpoints
            && !failures
                .iter()
                .any(|failure| matches!(failure, GraphFailure::Checkpoint { .. }))
        {
            // Finished, or the failed nodes to run again next time.
            let saved = snapshot(&state, &usage, &last, executions, &[], &[], &failures);
            if let Err(error) = checkpoints.save(id, &saved).await {
                tracing::warn!(%error, "could not save the graph run's end");
            }
        }
        let run = GraphRun {
            state,
            last,
            usage,
            elapsed: before + started.elapsed(),
        };
        if failures.is_empty() {
            Ok(run)
        } else {
            Err(GraphError { failures, run })
        }
    }

    /// The input of node `index`: the task, and the outputs that lead to it.
    fn input(&self, index: usize, state: &GraphState) -> String {
        let inner = &*self.inner;
        let mut sources: Vec<(&str, &str)> = Vec::new();
        for edge in inner.edges.iter().filter(|edge| edge.to == index) {
            let Some(from) = inner.nodes.get(edge.from) else {
                continue;
            };
            let from = from.id.as_str();
            if let Some(output) = state.outputs.get(from)
                && !sources.iter().any(|(seen, _)| *seen == from)
                && edge.holds(state)
            {
                sources.push((from, output));
            }
        }
        if sources.is_empty() {
            return state.task.clone();
        }
        let mut text = format!(
            "Original task: {}\n\nInputs from previous nodes:\n",
            state.task
        );
        for (from, output) in sources {
            let _ = write!(text, "\nFrom {from}:\n{output}\n");
        }
        text.trim_end().to_owned()
    }
}

impl Node for Graph {
    /// Streams the text of its nodes and passes on their questions. In a
    /// checkpointed graph run, the inner graph keeps its own progress (in
    /// its store, or else the outer graph's), so it resumes inside too.
    fn run<'a>(
        &'a self,
        input: String,
        io: &'a NodeIo<'a>,
    ) -> WasmBoxedFuture<'a, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let saving = io.saving().map(|saving| {
                let checkpoints = self.inner.checkpoints.clone();
                let checkpoints =
                    checkpoints.unwrap_or_else(|| saving.checkpoints.with_kind("graph"));
                Saving::new(checkpoints, saving.run_id.clone())
            });
            let emit = |event| match event {
                GraphEvent::Text { text, .. } => io.text(&text),
                GraphEvent::Question(question) => io.forward(question),
                _ => {}
            };
            let emit: Option<Emit<'_, GraphEvent>> = io.is_streaming().then_some(&emit);
            let run = self.run_task(input, saving, true, emit).await?;
            Ok(NodeOutput::new(run.answer()).with_usage(run.usage))
        })
    }
}
