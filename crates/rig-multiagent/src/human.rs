//! Asking a person: questions, the node that asks one, and the run clock that
//! stops while a person thinks.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio::sync::oneshot;

use crate::node::{Node, NodeError, NodeIo, NodeOutput};

/// A question for a person, reported by a streamed run. The part of the run
/// that asked waits until it is [answered](Self::answer); time spent waiting
/// does not count against the run's timeouts. Dropping it unanswered tells
/// the asker that nobody answered.
pub struct Question {
    from: String,
    question: String,
    context: String,
    reply: oneshot::Sender<String>,
    /// Stops the run clocks of every level the question passed through,
    /// until it is answered or dropped.
    _paused: Vec<Pause>,
}

impl std::fmt::Debug for Question {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Question")
            .field("from", &self.from)
            .field("question", &self.question)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl Question {
    pub(crate) fn new(
        from: impl Into<String>,
        question: impl Into<String>,
        context: impl Into<String>,
    ) -> (Self, oneshot::Receiver<String>) {
        let (reply, answer) = oneshot::channel();
        let question = Self {
            from: from.into(),
            question: question.into(),
            context: context.into(),
            reply,
            _paused: Vec::new(),
        };
        (question, answer)
    }

    /// Who asks: a graph node or swarm agent, with the nodes it sits in
    /// before it (`review/approve`, `team/planner`).
    pub fn from(&self) -> &str {
        &self.from
    }

    /// The question.
    pub fn question(&self) -> &str {
        &self.question
    }

    /// What the question is about: for a [`human`] node, its input (the
    /// work to approve, say). Empty when the asker gave none.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Answer it; the run carries on.
    pub fn answer(self, answer: impl Into<String>) {
        // The asker may be gone (its run stopped); nobody to tell then.
        let _ = self.reply.send(answer.into());
    }

    /// The same question, stopping `clock` until it is answered.
    pub(crate) fn pausing(mut self, clock: &Clock) -> Self {
        self._paused.push(clock.pause());
        self
    }

    /// The same question, as seen from the node `node` it came out of.
    pub(crate) fn via(mut self, node: &str) -> Self {
        self.from = format!("{node}/{}", self.from);
        self
    }
}

/// Wait for the answer to a question the asker sent.
pub(crate) async fn answer(answer: oneshot::Receiver<String>) -> Result<String, NodeError> {
    answer
        .await
        .map_err(|_| "the question was dismissed without an answer".into())
}

/// A graph node that asks a person; see [`human`].
pub struct HumanNode {
    question: String,
}

/// A graph node that asks a person `question`, with its input as the
/// question's [context](Question::context), and outputs their answer.
///
/// Only a [streamed](crate::Graph::stream) run can ask; a plain run fails at
/// this node. To gate work on an answer, put the condition on the edges into
/// the gated node, from the human node and from the work:
///
/// ```
/// use rig_multiagent::{Graph, GraphState, human, node_fn};
/// # let build = node_fn(|_: String| async { Ok("release notes".to_owned()) });
/// # let deploy = node_fn(|input: String| async move { Ok(input) });
/// let approved = |state: &GraphState| state.output("approve") == Some("yes");
/// let graph = Graph::builder()
///     .node("build", build)
///     .node("approve", human("Deploy this release? (yes/no)"))
///     .node("deploy", deploy)
///     .edge("build", "approve")
///     .edge_if("approve", "deploy", approved)
///     .edge_if("build", "deploy", approved) // so deploy gets the build too
///     .build()?;
/// # Ok::<(), rig_multiagent::BuildError>(())
/// ```
pub fn human(question: impl Into<String>) -> HumanNode {
    HumanNode {
        question: question.into(),
    }
}

impl Node for HumanNode {
    fn run<'a>(
        &'a self,
        input: String,
        io: &'a NodeIo<'a>,
    ) -> BoxFuture<'a, Result<NodeOutput, NodeError>> {
        Box::pin(async move { Ok(NodeOutput::new(io.ask(&self.question, input).await?)) })
    }
}

/// A run's clock: time that counts against its timeouts, which stops while
/// any question of the run waits for a person.
#[derive(Clone)]
pub(crate) struct Clock(Arc<Mutex<ClockState>>);

struct ClockState {
    /// Counted time from before a restart.
    offset: Duration,
    started: Instant,
    paused_total: Duration,
    paused_since: Option<Instant>,
    pauses: usize,
}

impl Clock {
    /// A clock that has already counted `offset`, for a resumed run.
    pub(crate) fn resumed(offset: Duration) -> Self {
        Self(Arc::new(Mutex::new(ClockState {
            offset,
            started: Instant::now(),
            paused_total: Duration::ZERO,
            paused_since: None,
            pauses: 0,
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ClockState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The time that counts so far.
    pub(crate) fn active(&self) -> Duration {
        let state = self.state();
        let now = Instant::now();
        let paused = state.paused_total
            + state
                .paused_since
                .map_or(Duration::ZERO, |since| now - since);
        state.offset + (now - state.started).saturating_sub(paused)
    }

    fn is_paused(&self) -> bool {
        self.state().pauses > 0
    }

    fn pause(&self) -> Pause {
        let mut state = self.state();
        state.pauses += 1;
        if state.pauses == 1 {
            state.paused_since = Some(Instant::now());
        }
        Pause(self.clone())
    }

    /// `future`'s output, or `None` once it has used `budget` of counted time.
    pub(crate) async fn within<F: Future>(&self, budget: Duration, future: F) -> Option<F::Output> {
        let start = self.active();
        let mut future = std::pin::pin!(future);
        loop {
            let left = budget.saturating_sub(self.active().saturating_sub(start));
            let paused = self.is_paused();
            if left.is_zero() && !paused {
                return None;
            }
            // While paused the counted time stands still: check back later.
            let wait = if paused {
                left.max(Duration::from_secs(1))
            } else {
                left
            };
            tokio::select! {
                output = &mut future => return Some(output),
                () = tokio::time::sleep(wait) => {}
            }
        }
    }
}

/// Holds a clock stopped; dropping it restarts the clock once no other
/// pause holds it.
pub(crate) struct Pause(Clock);

impl Drop for Pause {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.pauses -= 1;
        if state.pauses == 0
            && let Some(since) = state.paused_since.take()
        {
            state.paused_total += since.elapsed();
        }
    }
}
