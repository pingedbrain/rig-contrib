//! Nodes run along edges, in parallel where they can.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, join_all};
use futures::stream::BoxStream;
use rig_core::completion::Usage;
use tracing::Instrument;

use crate::BuildError;
use crate::events::{Emit, events};
use crate::node::{Node, NodeError, NodeOutput, OnText};

type Condition = Arc<dyn Fn(&GraphState) -> bool + Send + Sync>;

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
    execution_timeout: Duration,
    node_timeout: Duration,
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
    execution_timeout: Duration,
    node_timeout: Duration,
}

impl Graph {
    /// Start building a graph.
    pub fn builder() -> GraphBuilder {
        GraphBuilder {
            nodes: Vec::new(),
            edges: Vec::new(),
            entries: Vec::new(),
            max_node_executions: None,
            execution_timeout: Duration::from_secs(15 * 60),
            node_timeout: Duration::from_secs(5 * 60),
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
        condition: impl Fn(&GraphState) -> bool + Send + Sync + 'static,
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

    /// Check the nodes and edges.
    pub fn build(self) -> Result<Graph, BuildError> {
        if self.nodes.is_empty() {
            return Err(BuildError::Empty);
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
                execution_timeout: self.execution_timeout,
                node_timeout: self.node_timeout,
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
        incoming[edge.to] += 1;
    }
    let mut free: Vec<usize> = (0..nodes).filter(|&node| incoming[node] == 0).collect();
    let mut peeled = 0;
    while let Some(node) = free.pop() {
        peeled += 1;
        for edge in edges.iter().filter(|edge| edge.from == node) {
            incoming[edge.to] -= 1;
            if incoming[edge.to] == 0 {
                free.push(edge.to);
            }
        }
    }
    peeled < nodes
}

/// A run so far, as edge conditions see it.
#[derive(Debug, Clone, Default)]
pub struct GraphState {
    task: String,
    outputs: HashMap<String, String>,
    runs: HashMap<String, usize>,
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
    /// The run ended: the last event. Boxed, so the many text events stay small.
    Finished(Box<Result<GraphRun, GraphError>>),
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

impl Graph {
    /// Run the graph on `task`.
    pub async fn run(&self, task: impl Into<String>) -> Result<GraphRun, GraphError> {
        let span = tracing::info_span!("graph.run");
        self.run_task(task.into(), None).instrument(span).await
    }

    /// Run the graph on `task`, reporting each node as it starts, writes and
    /// finishes, and the outcome last.
    ///
    /// The run makes progress only while the stream is polled, and stops
    /// when the stream is dropped.
    pub fn stream(&self, task: impl Into<String>) -> BoxStream<'_, GraphEvent> {
        let task = task.into();
        events(
            move |sender| {
                async move {
                    let emit = move |event| {
                        let _ = sender.send(event);
                    };
                    self.run_task(task, Some(&emit)).await
                }
                .instrument(tracing::info_span!("graph.run"))
            },
            |outcome| GraphEvent::Finished(Box::new(outcome)),
        )
    }

    async fn run_task(
        &self,
        task: String,
        emit: Option<Emit<'_, GraphEvent>>,
    ) -> Result<GraphRun, GraphError> {
        let inner = &*self.inner;
        let started = Instant::now();
        let deadline = started + inner.execution_timeout;
        let mut state = GraphState {
            task,
            ..GraphState::default()
        };
        let mut usage = Usage::default();
        let mut failures = Vec::new();
        let mut last = Vec::new();
        let mut executions = 0;
        let mut ready = inner.entries.clone();
        while !ready.is_empty() {
            if let Some(limit) = inner.max_node_executions
                && executions + ready.len() > limit
            {
                failures.push(GraphFailure::MaxNodeExecutions { limit });
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                failures.push(GraphFailure::Timeout);
                break;
            }
            let budget = inner.node_timeout.min(remaining);
            executions += ready.len();
            let batch = ready.iter().map(|&index| {
                let node = &inner.nodes[index];
                let input = self.input(index, &state);
                let span = tracing::info_span!("graph.node", graph.node = %node.id);
                async move {
                    let Some(emit) = emit else {
                        let run = node.node.run(input).instrument(span);
                        return (index, tokio::time::timeout(budget, run).await);
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
                    let run = node.node.run_streaming(input, &on_text).instrument(span);
                    let outcome = tokio::time::timeout(budget, run).await;
                    let node = node.id.clone();
                    emit(match &outcome {
                        Ok(Ok(output)) => GraphEvent::NodeFinished {
                            node,
                            output: output.text.clone(),
                        },
                        Ok(Err(error)) => GraphEvent::NodeFailed {
                            node,
                            error: error.to_string(),
                        },
                        Err(_elapsed) => GraphEvent::NodeFailed {
                            node,
                            error: "did not finish in time".to_owned(),
                        },
                    });
                    (index, outcome)
                }
            });
            let mut finished = Vec::new();
            for (index, outcome) in join_all(batch).await {
                let id = inner.nodes[index].id.clone();
                *state.runs.entry(id.clone()).or_default() += 1;
                state.order.push(id.clone());
                match outcome {
                    Ok(Ok(output)) => {
                        let NodeOutput {
                            text, usage: spent, ..
                        } = output;
                        usage += spent;
                        state.outputs.insert(id, text);
                        finished.push(index);
                    }
                    Ok(Err(source)) => {
                        tracing::warn!(graph.node = %id, error = %source, "graph node failed");
                        failures.push(GraphFailure::Node { node: id, source });
                    }
                    Err(_elapsed) => failures.push(GraphFailure::NodeTimeout { node: id }),
                }
            }
            if !finished.is_empty() {
                last = finished
                    .iter()
                    .map(|&index| inner.nodes[index].id.clone())
                    .collect();
            }
            ready = Vec::new();
            for edge in &inner.edges {
                if finished.contains(&edge.from) && !ready.contains(&edge.to) && edge.holds(&state)
                {
                    ready.push(edge.to);
                }
            }
        }
        let run = GraphRun {
            state,
            last,
            usage,
            elapsed: started.elapsed(),
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
        let mut sources: Vec<&str> = Vec::new();
        for edge in inner.edges.iter().filter(|edge| edge.to == index) {
            let from = inner.nodes[edge.from].id.as_str();
            if state.outputs.contains_key(from) && !sources.contains(&from) && edge.holds(state) {
                sources.push(from);
            }
        }
        if sources.is_empty() {
            return state.task.clone();
        }
        let mut text = format!(
            "Original task: {}\n\nInputs from previous nodes:\n",
            state.task
        );
        for from in sources {
            let _ = write!(text, "\nFrom {from}:\n{}\n", state.outputs[from]);
        }
        text.trim_end().to_owned()
    }
}

impl Node for Graph {
    fn run(&self, input: String) -> BoxFuture<'_, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let run = Graph::run(self, input).await?;
            Ok(NodeOutput::new(run.answer()).with_usage(run.usage))
        })
    }

    /// Streams the text of its nodes.
    fn run_streaming<'a>(
        &'a self,
        input: String,
        on_text: OnText<'a>,
    ) -> BoxFuture<'a, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let emit = |event| {
                if let GraphEvent::Text { text, .. } = event {
                    on_text(&text);
                }
            };
            let run = self.run_task(input, Some(&emit)).await?;
            Ok(NodeOutput::new(run.answer()).with_usage(run.usage))
        })
    }
}
