//! Agents that pass the work to each other.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rig_agent::Agent;
use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_agent::completion::PromptError;
use rig_core::completion::Usage;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use rig_core::wasm_compat::{WasmBoxedFuture, WasmBoxedStream, WasmCompatSend, WasmCompatSync};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BuildError;
use crate::checkpoint::{CheckpointError, Checkpoints, Journal, Saving};
use crate::events::{Emit, events, stream_agent};
use crate::human::{Clock, Question, answer};
use crate::node::{Meter, Node, NodeError, NodeIo, NodeOutput, in_conversation, turn_budget};

/// The model calls one agent's run may make unless the swarm or graph sets
/// another number.
pub(crate) const DEFAULT_MAX_TURNS: usize = 8;

/// The name of the tool a swarm gives each of its agents.
pub const HANDOFF_TOOL: &str = "handoff_to_agent";

/// The name of the tool a swarm that [allows questions](SwarmBuilder::allow_questions)
/// gives its agents.
pub const ASK_TOOL: &str = "ask_human";

/// A team of agents that pass the work to each other.
///
/// The run starts with the [entry](SwarmBuilder::entry) agent. Each agent
/// sees the task, who worked on it before, what they shared, and the other
/// agents it can hand off to; it either answers, which ends the run with
/// that answer, or calls [`handoff_to_agent`](HANDOFF_TOOL), which ends its
/// turn and starts the agent it named. Every agent run starts fresh: what
/// one agent tells the next travels in the handoff message and context.
///
/// Cloning is cheap; clones share the agents.
#[derive(Clone)]
pub struct Swarm {
    inner: Arc<Inner>,
}

struct Inner {
    members: Vec<Arc<Member>>,
    /// The member the run starts with, one of `members`.
    entry: Arc<Member>,
    max_handoffs: usize,
    max_turns: usize,
    execution_timeout: Duration,
    agent_timeout: Duration,
    repetition: Option<Repetition>,
    questions: bool,
    checkpoints: Option<Checkpoints>,
}

struct Member {
    name: String,
    description: Option<String>,
    agent: Agent,
}

#[derive(Clone, Copy)]
struct Repetition {
    window: usize,
    min_unique: usize,
}

/// Builds a [`Swarm`].
#[must_use]
pub struct SwarmBuilder {
    agents: Vec<Agent>,
    entry: Option<String>,
    max_handoffs: usize,
    max_turns: usize,
    execution_timeout: Duration,
    agent_timeout: Duration,
    repetition: Option<Repetition>,
    questions: bool,
    checkpoints: Option<Checkpoints>,
}

impl Swarm {
    /// Start building a swarm.
    pub fn builder() -> SwarmBuilder {
        SwarmBuilder {
            agents: Vec::new(),
            entry: None,
            max_handoffs: 20,
            max_turns: DEFAULT_MAX_TURNS,
            execution_timeout: Duration::from_secs(15 * 60),
            agent_timeout: Duration::from_secs(5 * 60),
            repetition: None,
            questions: false,
            checkpoints: None,
        }
    }
}

impl SwarmBuilder {
    /// Add an agent. It needs a [name](rig_agent::AgentBuilder::name), which
    /// the others hand off to; its description tells them what it is for.
    ///
    /// The swarm adds the [`handoff_to_agent`](HANDOFF_TOOL) tool to the
    /// agent's tool set, which clones of the agent share.
    ///
    /// Each run of the agent gets [`max_turns_per_agent`](SwarmBuilder::max_turns_per_agent)
    /// model calls, or the agent's own
    /// [`default_max_turns`](rig_agent::AgentBuilder::default_max_turns) if
    /// that is higher. Rig's own default of one turn is never used here, so a
    /// refused handoff, or a tool whose result the agent must read, gets its
    /// follow-up answer.
    pub fn agent(mut self, agent: Agent) -> Self {
        self.agents.push(agent);
        self
    }

    /// The agent the run starts with. Defaults to the first one added.
    pub fn entry(mut self, name: impl Into<String>) -> Self {
        self.entry = Some(name.into());
        self
    }

    /// How many handoffs a run may make before it fails. Defaults to 20.
    pub fn max_handoffs(mut self, max_handoffs: usize) -> Self {
        self.max_handoffs = max_handoffs;
        self
    }

    /// How many model calls one agent's run may make, counting the first call
    /// and every follow-up after a tool call. Applies to each agent run; an
    /// agent whose own `default_max_turns` is higher keeps its own. Defaults
    /// to 8; zero fails the build with [`BuildError::ZeroTurns`]. A handoff
    /// ends the run at once, so the budget only has to cover the tool calls an
    /// agent makes before it answers or hands off.
    pub fn max_turns_per_agent(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// How long a whole run may take. Defaults to 15 minutes.
    pub fn execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    /// How long one agent's turn may take. Defaults to 5 minutes.
    pub fn agent_timeout(mut self, timeout: Duration) -> Self {
        self.agent_timeout = timeout;
        self
    }

    /// Fail a run whose last `window` turns went to fewer than `min_unique`
    /// distinct agents: two agents passing the work back and forth. Off by
    /// default.
    pub fn detect_repetition(mut self, window: usize, min_unique: usize) -> Self {
        self.repetition = Some(Repetition { window, min_unique });
        self
    }

    /// Give the agents an [`ask_human`](ASK_TOOL) tool, to ask the person
    /// behind a [streamed](Swarm::stream) run for a decision or a fact. The
    /// agent waits for the answer within its turn, and the run's timeouts
    /// stop meanwhile. In a plain run nobody can answer, and the tool says
    /// so to the agent.
    pub fn allow_questions(mut self) -> Self {
        self.questions = true;
        self
    }

    /// Keep the progress of runs started with [`run_as`](Swarm::run_as) or
    /// [`stream_as`](Swarm::stream_as) in `store` (any rig-memory store: a
    /// `DirStore`, a database), so a run that stopped resumes where it was.
    pub fn checkpoints(
        mut self,
        store: impl rig_memory::AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
    ) -> Self {
        self.checkpoints = Some(Checkpoints::new(store, "swarm"));
        self
    }

    /// Check the agents and give each one the handoff tool.
    pub fn build(self) -> Result<Swarm, BuildError> {
        if self.agents.is_empty() {
            return Err(BuildError::Empty);
        }
        if self.max_turns == 0 {
            return Err(BuildError::ZeroTurns);
        }
        let mut members: Vec<Member> = Vec::with_capacity(self.agents.len());
        for agent in self.agents {
            let name = agent.name().ok_or(BuildError::UnnamedAgent)?.to_owned();
            if members.iter().any(|member| member.name == name) {
                return Err(BuildError::DuplicateName(name));
            }
            members.push(Member {
                description: agent.description().map(str::to_owned),
                name,
                agent,
            });
        }
        let members: Vec<Arc<Member>> = members.into_iter().map(Arc::new).collect();
        let entry = match &self.entry {
            None => members.first().ok_or(BuildError::Empty)?,
            Some(name) => members
                .iter()
                .find(|member| &member.name == name)
                .ok_or_else(|| BuildError::UnknownName(name.clone()))?,
        }
        .clone();
        for member in &members {
            if members.len() > 1 {
                member.agent.tool_server_handle().add_tool(HandoffTool);
            }
            if self.questions {
                member.agent.tool_server_handle().add_tool(AskTool);
            }
        }
        Ok(Swarm {
            inner: Arc::new(Inner {
                members,
                entry,
                max_handoffs: self.max_handoffs,
                max_turns: self.max_turns,
                execution_timeout: self.execution_timeout,
                agent_timeout: self.agent_timeout,
                repetition: self.repetition,
                questions: self.questions,
                checkpoints: self.checkpoints,
            }),
        })
    }
}

/// One agent passing the work to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    /// The agent that handed off.
    pub from: String,
    /// The agent it handed off to.
    pub to: String,
    /// What it asked the next agent to do.
    pub message: String,
    /// What it shared with the agents after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

/// A swarm run that ended with an answer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SwarmRun {
    /// The final answer.
    pub answer: String,
    /// The agent that gave it.
    pub agent: String,
    /// Every agent turn, in order, the last one included.
    pub agents: Vec<String>,
    /// Every handoff, in order.
    pub handoffs: Vec<Handoff>,
    /// The model tokens all the agents spent.
    pub usage: Usage,
    /// How long the run took.
    pub elapsed: Duration,
}

/// A swarm run that ended without an answer, with what happened before.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
#[non_exhaustive]
pub struct SwarmError {
    /// Why the run stopped.
    pub reason: SwarmFailure,
    /// Every agent turn, in order, the failed one included.
    pub agents: Vec<String>,
    /// Every handoff, in order.
    pub handoffs: Vec<Handoff>,
    /// The model tokens all the agents spent.
    pub usage: Usage,
}

/// Why a swarm run stopped without an answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SwarmFailure {
    /// An agent's run failed.
    #[error("agent {agent:?} failed: {source}")]
    Agent {
        /// The agent.
        agent: String,
        /// Its run's error.
        #[source]
        source: Box<PromptError>,
    },
    /// An agent's turn, or the whole run, ran out of time.
    #[error("agent {agent:?} did not finish in time")]
    Timeout {
        /// The agent whose turn was cut short.
        agent: String,
    },
    /// The agents handed off more times than [`SwarmBuilder::max_handoffs`].
    #[error("the swarm reached its limit of {limit} handoffs")]
    MaxHandoffs {
        /// The limit.
        limit: usize,
    },
    /// The checkpoint could not be read or written.
    #[error("checkpoint: {error}")]
    Checkpoint {
        /// What went wrong.
        #[source]
        error: CheckpointError,
    },
    /// The work kept bouncing among a few agents; see
    /// [`SwarmBuilder::detect_repetition`].
    #[error("the last {window} turns went to only {unique} agents")]
    Repetitive {
        /// How many recent turns were checked.
        window: usize,
        /// How many distinct agents they went to.
        unique: usize,
    },
}

/// What a [streamed](Swarm::stream) swarm run reports, as it happens.
#[derive(Debug)]
#[non_exhaustive]
pub enum SwarmEvent {
    /// An agent's turn started.
    AgentStarted {
        /// The agent.
        agent: String,
    },
    /// Text an agent's model wrote. A turn that ends in a handoff may write
    /// some before calling the tool; the final answer is the last turn's.
    Text {
        /// The agent.
        agent: String,
        /// The new text.
        text: String,
    },
    /// The run resumed from its checkpoint: the first event of a resumed
    /// [`stream_as`](Swarm::stream_as) run.
    Resumed {
        /// The agent whose turn starts the resumed run.
        agent: String,
        /// The handoffs made before the run stopped.
        handoffs: usize,
    },
    /// An agent passed the work on.
    Handoff(Handoff),
    /// An agent asks a person; it waits for the [answer](Question::answer),
    /// and the run's timeouts stop meanwhile. See [`SwarmBuilder::allow_questions`].
    Question(Question),
    /// The run ended: the last event. Boxed, so the many text events stay small.
    Finished(Box<Result<SwarmRun, SwarmError>>),
}

/// What a run has done so far.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Trail {
    agents: Vec<String>,
    handoffs: Vec<Handoff>,
    shared: Vec<(String, Value)>,
    usage: Usage,
}

/// A run's checkpoint: what it did, and whose turn is next.
#[derive(Serialize, Deserialize)]
struct Saved {
    task: String,
    finished: bool,
    /// The answer, once finished.
    #[serde(default)]
    answer: Option<String>,
    /// The agent whose turn comes next.
    current: String,
    message: Option<String>,
    trail: Trail,
    active_ms: u64,
    elapsed_ms: u64,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Trail {
    fn fail(self, reason: SwarmFailure) -> SwarmError {
        SwarmError {
            reason,
            agents: self.agents,
            handoffs: self.handoffs,
            usage: self.usage,
        }
    }
}

/// The span a swarm run is in. Its totals are recorded when the run ends:
/// tokens under `swarm.usage.*` (not `gen_ai.usage.*`, which the model calls
/// under it carry, so backends do not count them twice).
fn run_span(entry: &str, run_id: Option<&str>) -> tracing::Span {
    tracing::info_span!(
        "swarm.run",
        swarm.entry = %entry,
        swarm.run_id = run_id,
        swarm.agent = tracing::field::Empty,
        swarm.handoffs = tracing::field::Empty,
        swarm.usage.input_tokens = tracing::field::Empty,
        swarm.usage.output_tokens = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    )
}

/// Record how a run ended on its span.
fn record_outcome(span: &tracing::Span, outcome: &Result<SwarmRun, SwarmError>) {
    let (handoffs, usage) = match outcome {
        Ok(run) => (run.handoffs.len(), run.usage),
        Err(error) => (error.handoffs.len(), error.usage),
    };
    span.record("swarm.handoffs", handoffs);
    if let Some(tokens) = usage.input_tokens {
        span.record("swarm.usage.input_tokens", tokens);
    }
    if let Some(tokens) = usage.output_tokens {
        span.record("swarm.usage.output_tokens", tokens);
    }
    match outcome {
        Ok(run) => {
            span.record("swarm.agent", run.agent.as_str());
        }
        Err(error) => {
            span.record("otel.status_code", "ERROR");
            span.record("error.type", error.reason.kind());
        }
    }
}

impl SwarmFailure {
    /// A short, stable name for the failure, for `error.type`.
    fn kind(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "agent_failed",
            Self::Timeout { .. } => "timeout",
            Self::MaxHandoffs { .. } => "max_handoffs",
            Self::Repetitive { .. } => "repetitive_handoffs",
            Self::Checkpoint { .. } => "checkpoint",
        }
    }
}

impl Swarm {
    /// Run the swarm on `task` until an agent answers.
    pub async fn run(&self, task: impl Into<String>) -> Result<SwarmRun, SwarmError> {
        let span = run_span(&self.inner.entry.name, None);
        let outcome = self
            .run_task(task.into(), None, false, None)
            .instrument(span.clone())
            .await;
        record_outcome(&span, &outcome);
        outcome
    }

    /// Run the swarm as run `run_id`: resume it from its last checkpoint if
    /// it has one and did not finish, or else start it on `task`. Progress
    /// is saved before every turn, so a run that stopped (the process
    /// restarted, an agent failed) carries on with the turn it was on. Each
    /// turn keeps a journal of its tool results (answers to `ask_human`
    /// included), so a turn run again replays them rather than running the
    /// tools twice.
    ///
    /// Needs [`checkpoints`](SwarmBuilder::checkpoints); without them it is
    /// [`run`](Self::run). A `run_id` is 1-128 of `[A-Za-z0-9_-]`.
    pub async fn run_as(
        &self,
        run_id: &str,
        task: impl Into<String>,
    ) -> Result<SwarmRun, SwarmError> {
        let span = run_span(&self.inner.entry.name, Some(run_id));
        let outcome = self
            .run_task(task.into(), self.saving(Some(run_id)), false, None)
            .instrument(span.clone())
            .await;
        record_outcome(&span, &outcome);
        outcome
    }

    /// Run the swarm on `task`, reporting each turn, the text its model
    /// writes and each handoff as they happen, and the outcome last.
    ///
    /// The run makes progress only while the stream is polled, and stops
    /// when the stream is dropped.
    ///
    /// ```no_run
    /// # async fn show(swarm: rig_multiagent::Swarm) {
    /// use futures::StreamExt;
    /// use rig_multiagent::SwarmEvent;
    ///
    /// let mut events = swarm.stream("Fix the flaky test");
    /// while let Some(event) = events.next().await {
    ///     match event {
    ///         SwarmEvent::AgentStarted { agent } => println!("\n[{agent}]"),
    ///         SwarmEvent::Text { text, .. } => print!("{text}"),
    ///         SwarmEvent::Handoff(handoff) => println!("\n-> {}", handoff.to),
    ///         SwarmEvent::Finished(outcome) => println!("\n{outcome:?}"),
    ///         _ => {}
    ///     }
    /// }
    /// # }
    /// ```
    pub fn stream(&self, task: impl Into<String>) -> WasmBoxedStream<'_, SwarmEvent> {
        self.stream_events(task.into(), None)
    }

    /// [`stream`](Self::stream) as run `run_id`, resuming it as
    /// [`run_as`](Self::run_as) does. A resumed run starts with
    /// [`SwarmEvent::Resumed`].
    pub fn stream_as<'a>(
        &'a self,
        run_id: &'a str,
        task: impl Into<String>,
    ) -> WasmBoxedStream<'a, SwarmEvent> {
        self.stream_events(task.into(), Some(run_id))
    }

    fn stream_events<'a>(
        &'a self,
        task: String,
        run_id: Option<&'a str>,
    ) -> WasmBoxedStream<'a, SwarmEvent> {
        let span = run_span(&self.inner.entry.name, run_id);
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
            |outcome| SwarmEvent::Finished(Box::new(outcome)),
        )
    }

    /// Where run `run_id` keeps its progress, if the swarm has a store.
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
        emit: Option<Emit<'_, SwarmEvent>>,
    ) -> Result<SwarmRun, SwarmError> {
        let inner = &*self.inner;
        let started = Instant::now();
        let checkpoints = saving
            .as_ref()
            .map(|saving| (saving.run_id.as_str(), &saving.checkpoints));
        let saved = match checkpoints {
            Some((id, checkpoints)) => match checkpoints.load::<Saved>(id).await {
                Ok(Some(Saved {
                    finished: true,
                    answer: Some(answer),
                    current,
                    trail,
                    elapsed_ms,
                    ..
                })) if nested => {
                    return Ok(SwarmRun {
                        answer,
                        agent: current,
                        agents: trail.agents,
                        handoffs: trail.handoffs,
                        usage: trail.usage,
                        elapsed: Duration::from_millis(elapsed_ms),
                    });
                }
                Ok(saved) => saved.filter(|saved| !saved.finished),
                Err(error) => {
                    return Err(Trail::default().fail(SwarmFailure::Checkpoint { error }));
                }
            },
            None => None,
        };
        let (task, mut trail, mut current, mut message, clock, before) = match saved {
            None => (
                task,
                Trail::default(),
                Arc::clone(&inner.entry),
                None,
                Clock::resumed(Duration::ZERO),
                Duration::ZERO,
            ),
            Some(saved) => {
                let Some(current) = inner
                    .members
                    .iter()
                    .find(|member| member.name == saved.current)
                    .cloned()
                else {
                    let error = CheckpointError::UnknownAgent(saved.current);
                    return Err(saved.trail.fail(SwarmFailure::Checkpoint { error }));
                };
                tracing::info!(agent = %saved.current, handoffs = saved.trail.handoffs.len(), "swarm run resumed");
                if let Some(emit) = emit {
                    emit(SwarmEvent::Resumed {
                        agent: saved.current.clone(),
                        handoffs: saved.trail.handoffs.len(),
                    });
                }
                let clock = Clock::resumed(Duration::from_millis(saved.active_ms));
                (
                    saved.task,
                    saved.trail,
                    current,
                    saved.message,
                    clock,
                    Duration::from_millis(saved.elapsed_ms),
                )
            }
        };
        let snapshot = |trail: &Trail,
                        current: &str,
                        message: &Option<String>,
                        answer: Option<String>| Saved {
            task: task.clone(),
            finished: answer.is_some(),
            answer,
            current: current.to_owned(),
            message: message.clone(),
            trail: trail.clone(),
            active_ms: millis(clock.active()),
            elapsed_ms: millis(before + started.elapsed()),
        };
        loop {
            let member = Arc::clone(&current);
            if let Some((id, checkpoints)) = checkpoints {
                let saved = snapshot(&trail, &member.name, &message, None);
                if let Err(error) = checkpoints.save(id, &saved).await {
                    return Err(trail.fail(SwarmFailure::Checkpoint { error }));
                }
            }
            trail.agents.push(member.name.clone());
            if let Some(Repetition { window, min_unique }) = inner.repetition
                && window > 0
                && trail.agents.len() >= window
            {
                let recent = trail.agents.iter().rev().take(window);
                let unique = recent.collect::<HashSet<_>>().len();
                if unique < min_unique {
                    return Err(trail.fail(SwarmFailure::Repetitive { window, unique }));
                }
            }
            let budget = inner
                .agent_timeout
                .min(inner.execution_timeout.saturating_sub(clock.active()));
            let input = self.input(&member, &task, message.as_deref(), &trail);
            let turn = Arc::new(Turn {
                agent: member.name.clone(),
                others: inner
                    .members
                    .iter()
                    .filter(|other| other.name != member.name)
                    .cloned()
                    .collect(),
                handoff: Mutex::new(None),
                questions: Mutex::new(None),
            });
            let meter = Meter::default();
            // Each turn is a conversation of its own: one cache key for it,
            // the same when a resumed run repeats the turn.
            let part = saving
                .as_ref()
                .map(|saving| saving.part("journal", "turn", trail.agents.len()));
            let key = part.as_ref().map(|part| part.run_id.clone());
            let prompt = member
                .agent
                .prompt(input)
                .max_turns(turn_budget(&member.agent, inner.max_turns));
            let mut runner = in_conversation(prompt, &member.agent, key)
                .tool_context(ToolContext::new().with_scope(turn.clone()))
                .add_hook(meter.clone());
            if let Some(journal) = part {
                // A resumed turn replays the tools it ran before, answers to
                // ask_human included, rather than running them again.
                match Journal::open(journal).await {
                    Ok(journal) => runner = runner.add_hook(journal),
                    Err(error) => return Err(trail.fail(SwarmFailure::Checkpoint { error })),
                }
            }
            let runner = runner.add_hook(StopOnHandoff(turn.clone()));
            let run = async {
                let Some(emit) = emit else {
                    return runner.run().await;
                };
                emit(SwarmEvent::AgentStarted {
                    agent: member.name.clone(),
                });
                let on_text = |text: &str| {
                    emit(SwarmEvent::Text {
                        agent: member.name.clone(),
                        text: text.to_owned(),
                    })
                };
                // Questions the agent's tool asks reach the run's reader
                // while the agent waits.
                let (asks, mut asked) = mpsc::unbounded_channel();
                if inner.questions {
                    *turn.asks() = Some(asks);
                }
                let mut agent = std::pin::pin!(stream_agent(runner, &on_text));
                loop {
                    tokio::select! {
                        outcome = &mut agent => break outcome,
                        Some(question) = asked.recv() => {
                            // The question's text stays out of telemetry, as Rig
                            // keeps prompts out unless content recording is on.
                            tracing::info!(swarm.agent = %question.from(), "human.question");
                            emit(SwarmEvent::Question(question.pausing(&clock)));
                        }
                    }
                }
            }
            .instrument(tracing::info_span!("swarm.agent", swarm.agent = %member.name));
            let outcome = clock.within(budget, run).await;
            trail.usage += meter.total();
            let handoff = turn.take();
            match (outcome, handoff) {
                (None, _) => {
                    let agent = member.name.clone();
                    return Err(trail.fail(SwarmFailure::Timeout { agent }));
                }
                (Some(_), Some((handoff, next))) => {
                    if trail.handoffs.len() >= inner.max_handoffs {
                        let limit = inner.max_handoffs;
                        return Err(trail.fail(SwarmFailure::MaxHandoffs { limit }));
                    }
                    // A span event of the run: where the work went, and why.
                    tracing::info!(
                        swarm.from = %handoff.from,
                        swarm.to = %handoff.to,
                        swarm.message = %handoff.message,
                        "swarm.handoff"
                    );
                    current = next;
                    if let Some(context) = &handoff.context {
                        trail.shared.push((handoff.from.clone(), context.clone()));
                    }
                    message = Some(handoff.message.clone());
                    if let Some(emit) = emit {
                        emit(SwarmEvent::Handoff(handoff.clone()));
                    }
                    trail.handoffs.push(handoff);
                }
                (Some(Ok(response)), None) => {
                    if let Some((id, checkpoints)) = checkpoints {
                        // The answer stands even if the mark is lost; the
                        // next run_as then repeats the last turn.
                        let saved =
                            snapshot(&trail, &member.name, &message, Some(response.output()));
                        if let Err(error) = checkpoints.save(id, &saved).await {
                            tracing::warn!(%error, "could not mark the swarm run finished");
                        }
                    }
                    return Ok(SwarmRun {
                        answer: response.output(),
                        agent: member.name.clone(),
                        agents: trail.agents,
                        handoffs: trail.handoffs,
                        usage: trail.usage,
                        elapsed: before + started.elapsed(),
                    });
                }
                (Some(Err(source)), None) => {
                    let agent = member.name.clone();
                    let source = Box::new(source);
                    return Err(trail.fail(SwarmFailure::Agent { agent, source }));
                }
            }
        }
    }

    /// The prompt an agent's turn starts with.
    fn input(&self, current: &Member, task: &str, message: Option<&str>, trail: &Trail) -> String {
        let members = &self.inner.members;
        let mut text = String::new();
        if let (Some(message), Some(last)) = (message, trail.handoffs.last()) {
            let _ = write!(text, "Handoff from {}: {message}\n\n", last.from);
        }
        let _ = write!(text, "User request: {task}\n\n");
        let before: &[String] = trail
            .agents
            .split_last()
            .map(|(_, before)| before)
            .unwrap_or_default();
        if !before.is_empty() {
            let _ = write!(
                text,
                "Agents who worked on this so far: {}\n\n",
                before.join(" -> ")
            );
        }
        if !trail.shared.is_empty() {
            text.push_str("Shared by previous agents:\n");
            for (agent, context) in &trail.shared {
                let _ = writeln!(text, "- {agent}: {context}");
            }
            text.push('\n');
        }
        let others: Vec<&Member> = members
            .iter()
            .filter(|member| member.name != current.name)
            .map(|member| &**member)
            .collect();
        if !others.is_empty() {
            text.push_str("Other agents you can hand off to:\n");
            for other in others {
                match &other.description {
                    Some(description) => {
                        let _ = writeln!(text, "- {}: {description}", other.name);
                    }
                    None => {
                        let _ = writeln!(text, "- {}", other.name);
                    }
                }
            }
            let _ = write!(
                text,
                "\nTo pass the work on, call {HANDOFF_TOOL} with the agent's name and a message \
                 saying what it should do; your turn ends there. If you answer instead, your \
                 answer is the final one."
            );
        }
        if self.inner.questions {
            let _ = write!(
                text,
                "\n\nIf you need a decision or a fact only the user has, call {ASK_TOOL} \
                 with your question and wait for the answer."
            );
        }
        text.trim_end().to_owned()
    }
}

impl Node for Swarm {
    /// Streams every agent's text, handing-off turns included, and passes on
    /// their questions. In a checkpointed graph run, the swarm keeps its own
    /// progress (in its store, or else the graph's), so a resumed graph
    /// resumes the swarm inside the node too.
    fn run<'a>(
        &'a self,
        input: String,
        io: &'a NodeIo<'a>,
    ) -> WasmBoxedFuture<'a, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let saving = io.saving().map(|saving| {
                let checkpoints = self.inner.checkpoints.clone();
                let checkpoints =
                    checkpoints.unwrap_or_else(|| saving.checkpoints.with_kind("swarm"));
                Saving::new(checkpoints, saving.run_id.clone())
            });
            let emit = |event| match event {
                SwarmEvent::Text { text, .. } => io.text(&text),
                SwarmEvent::Question(question) => io.forward(question),
                _ => {}
            };
            let emit: Option<Emit<'_, SwarmEvent>> = io.is_streaming().then_some(&emit);
            let run = self.run_task(input, saving, true, emit).await?;
            Ok(NodeOutput::new(run.answer).with_usage(run.usage))
        })
    }
}

/// One agent's turn, as the handoff tool and hook see it.
struct Turn {
    agent: String,
    /// The members it can hand off to.
    others: Vec<Arc<Member>>,
    /// The handoff it made, with the member it hands off to.
    handoff: Mutex<Option<(Handoff, Arc<Member>)>>,
    /// Where the ask tool sends questions; `None` when nobody can answer.
    questions: Mutex<Option<mpsc::UnboundedSender<Question>>>,
}

impl Turn {
    fn asks(&self) -> std::sync::MutexGuard<'_, Option<mpsc::UnboundedSender<Question>>> {
        self.questions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<(Handoff, Arc<Member>)>> {
        self.handoff.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> Option<(Handoff, Arc<Member>)> {
        self.slot().take()
    }
}

#[derive(Deserialize)]
struct HandoffArgs {
    agent_name: String,
    message: String,
    #[serde(default)]
    context: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
enum HandoffError {
    #[error("{HANDOFF_TOOL} works only during a swarm run")]
    NotInSwarm,
    #[error("you are {0}; hand off to another agent or answer yourself")]
    ToSelf(String),
    #[error("no agent is named {0:?}; you can hand off to: {1}")]
    Unknown(String, String),
    #[error("you already handed off to {0} this turn")]
    Twice(String),
}

/// The tool a swarm adds to its agents. It records the handoff for the turn
/// it runs in (found through the tool context), and [`StopOnHandoff`] ends
/// the turn.
struct HandoffTool;

impl Tool for HandoffTool {
    const NAME: &'static str = HANDOFF_TOOL;
    type Args = HandoffArgs;
    type Output = String;
    type Error = HandoffError;

    fn description(&self) -> String {
        "Pass the work to another agent of the swarm, with a message saying what it should do \
         next and why, and optionally context to share with the agents after you. Your turn \
         ends when you call this. Answer directly yourself when you can; hand off only work \
         that another agent must do. Do not hand back to the agent that just handed the work \
         to you unless you have new information for it."
            .into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_name": {"type": "string", "description": "The agent to hand off to."},
                "message": {"type": "string", "description": "What the next agent should do, and why."},
                "context": {"type": "object", "description": "Facts to share with the agents after you."}
            },
            "required": ["agent_name", "message"]
        })
    }

    /// The messages name the agents to pick from, so the model sees them.
    fn map_error(&self, error: HandoffError) -> ToolExecutionError {
        let feedback = error.to_string();
        ToolExecutionError::from_error(error).with_model_feedback(feedback)
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: HandoffArgs,
    ) -> Result<String, HandoffError> {
        let turn = context.scope::<Turn>().ok_or(HandoffError::NotInSwarm)?;
        if args.agent_name == turn.agent {
            return Err(HandoffError::ToSelf(turn.agent.clone()));
        }
        let Some(next) = turn
            .others
            .iter()
            .find(|other| other.name == args.agent_name)
        else {
            let names: Vec<&str> = turn
                .others
                .iter()
                .map(|other| other.name.as_str())
                .collect();
            return Err(HandoffError::Unknown(args.agent_name, names.join(", ")));
        };
        let mut slot = turn.slot();
        if let Some((earlier, _)) = &*slot {
            return Err(HandoffError::Twice(earlier.to.clone()));
        }
        let reply = format!("Handing off to {}.", args.agent_name);
        let handoff = Handoff {
            from: turn.agent.clone(),
            to: args.agent_name,
            message: args.message,
            context: args.context.filter(|context| !context.is_null()),
        };
        *slot = Some((handoff, Arc::clone(next)));
        Ok(reply)
    }
}

#[derive(Deserialize)]
struct AskArgs {
    question: String,
}

#[derive(Debug, thiserror::Error)]
enum AskError {
    #[error("{ASK_TOOL} works only during a swarm run")]
    NotInSwarm,
    #[error("nobody can answer questions in this run; decide on your own")]
    Nobody,
    #[error("{0}; decide on your own")]
    Unanswered(NodeError),
}

/// The tool a swarm that allows questions adds to its agents: it sends the
/// question to the run's reader and waits for the answer, within the turn.
struct AskTool;

impl Tool for AskTool {
    const NAME: &'static str = ASK_TOOL;
    type Args = AskArgs;
    type Output = String;
    type Error = AskError;

    fn description(&self) -> String {
        "Ask the user a question (a decision, a missing fact, an approval) and wait for the \
         answer."
            .into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question, standing on its own."}
            },
            "required": ["question"]
        })
    }

    fn map_error(&self, error: AskError) -> ToolExecutionError {
        let feedback = error.to_string();
        ToolExecutionError::from_error(error).with_model_feedback(feedback)
    }

    async fn call(&self, context: &mut ToolContext, args: AskArgs) -> Result<String, AskError> {
        let turn = context.scope::<Turn>().ok_or(AskError::NotInSwarm)?;
        let (question, reply) = Question::new(turn.agent.clone(), args.question, "");
        let sent = turn.asks().as_ref().map(|asks| asks.send(question));
        if !matches!(sent, Some(Ok(()))) {
            return Err(AskError::Nobody);
        }
        answer(reply).await.map_err(AskError::Unanswered)
    }
}

/// Ends an agent's turn once it has handed off, so it does not keep working
/// on what the next agent now owns.
struct StopOnHandoff(Arc<Turn>);

impl AgentHook for StopOnHandoff {
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if event.tool_result().is_some() && self.0.slot().is_some() {
            OutcomeAction::stop("handed off")
        } else {
            OutcomeAction::proceed()
        }
    }
}
