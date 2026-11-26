//! Agents that pass the work to each other.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use rig_agent::Agent;
use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_agent::completion::PromptError;
use rig_core::completion::Usage;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::BuildError;
use crate::node::{Meter, Node, NodeError, NodeOutput};

/// The name of the tool a swarm gives each of its agents.
pub const HANDOFF_TOOL: &str = "handoff_to_agent";

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
    members: Vec<Member>,
    entry: usize,
    max_handoffs: usize,
    execution_timeout: Duration,
    agent_timeout: Duration,
    repetition: Option<Repetition>,
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
    execution_timeout: Duration,
    agent_timeout: Duration,
    repetition: Option<Repetition>,
}

impl Swarm {
    /// Start building a swarm.
    pub fn builder() -> SwarmBuilder {
        SwarmBuilder {
            agents: Vec::new(),
            entry: None,
            max_handoffs: 20,
            execution_timeout: Duration::from_secs(15 * 60),
            agent_timeout: Duration::from_secs(5 * 60),
            repetition: None,
        }
    }
}

impl SwarmBuilder {
    /// Add an agent. It needs a [name](rig_agent::AgentBuilder::name), which
    /// the others hand off to; its description tells them what it is for.
    ///
    /// The swarm adds the [`handoff_to_agent`](HANDOFF_TOOL) tool to the
    /// agent's tool set, which clones of the agent share.
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

    /// Check the agents and give each one the handoff tool.
    pub fn build(self) -> Result<Swarm, BuildError> {
        if self.agents.is_empty() {
            return Err(BuildError::Empty);
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
        let entry = match &self.entry {
            None => 0,
            Some(name) => members
                .iter()
                .position(|member| &member.name == name)
                .ok_or_else(|| BuildError::UnknownName(name.clone()))?,
        };
        if members.len() > 1 {
            for member in &members {
                member.agent.tool_server_handle().add_tool(HandoffTool);
            }
        }
        Ok(Swarm {
            inner: Arc::new(Inner {
                members,
                entry,
                max_handoffs: self.max_handoffs,
                execution_timeout: self.execution_timeout,
                agent_timeout: self.agent_timeout,
                repetition: self.repetition,
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

/// What a run has done so far.
#[derive(Default)]
struct Trail {
    agents: Vec<String>,
    handoffs: Vec<Handoff>,
    shared: Vec<(String, Value)>,
    usage: Usage,
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

impl Swarm {
    /// Run the swarm on `task` until an agent answers.
    pub async fn run(&self, task: impl Into<String>) -> Result<SwarmRun, SwarmError> {
        let entry = &self.inner.members[self.inner.entry].name;
        let span = tracing::info_span!("swarm.run", swarm.entry = %entry);
        self.run_task(task.into()).instrument(span).await
    }

    async fn run_task(&self, task: String) -> Result<SwarmRun, SwarmError> {
        let inner = &*self.inner;
        let started = Instant::now();
        let deadline = started + inner.execution_timeout;
        let mut trail = Trail::default();
        let mut current = inner.entry;
        let mut message: Option<String> = None;
        loop {
            let member = &inner.members[current];
            trail.agents.push(member.name.clone());
            if let Some(Repetition { window, min_unique }) = inner.repetition
                && window > 0
                && trail.agents.len() >= window
            {
                let recent = &trail.agents[trail.agents.len() - window..];
                let unique = recent.iter().collect::<HashSet<_>>().len();
                if unique < min_unique {
                    return Err(trail.fail(SwarmFailure::Repetitive { window, unique }));
                }
            }
            let budget = inner
                .agent_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            let input = self.input(current, &task, message.as_deref(), &trail);
            let turn = Arc::new(Turn {
                agent: member.name.clone(),
                others: inner
                    .members
                    .iter()
                    .filter(|other| other.name != member.name)
                    .map(|other| other.name.clone())
                    .collect(),
                handoff: Mutex::new(None),
            });
            let meter = Meter::default();
            let run = member
                .agent
                .prompt(input)
                .tool_context(ToolContext::new().with_scope(turn.clone()))
                .add_hook(meter.clone())
                .add_hook(StopOnHandoff(turn.clone()))
                .run()
                .instrument(tracing::info_span!("swarm.agent", swarm.agent = %member.name));
            let outcome = tokio::time::timeout(budget, run).await;
            trail.usage += meter.total();
            let handoff = turn.take();
            match (outcome, handoff) {
                (Err(_elapsed), _) => {
                    let agent = member.name.clone();
                    return Err(trail.fail(SwarmFailure::Timeout { agent }));
                }
                (Ok(_), Some(handoff)) => {
                    if trail.handoffs.len() >= inner.max_handoffs {
                        let limit = inner.max_handoffs;
                        return Err(trail.fail(SwarmFailure::MaxHandoffs { limit }));
                    }
                    tracing::debug!(from = %handoff.from, to = %handoff.to, "swarm handoff");
                    current = inner
                        .members
                        .iter()
                        .position(|other| other.name == handoff.to)
                        .expect("the handoff tool accepts only members");
                    if let Some(context) = &handoff.context {
                        trail.shared.push((handoff.from.clone(), context.clone()));
                    }
                    message = Some(handoff.message.clone());
                    trail.handoffs.push(handoff);
                }
                (Ok(Ok(response)), None) => {
                    return Ok(SwarmRun {
                        answer: response.output(),
                        agent: member.name.clone(),
                        agents: trail.agents,
                        handoffs: trail.handoffs,
                        usage: trail.usage,
                        elapsed: started.elapsed(),
                    });
                }
                (Ok(Err(source)), None) => {
                    let agent = member.name.clone();
                    let source = Box::new(source);
                    return Err(trail.fail(SwarmFailure::Agent { agent, source }));
                }
            }
        }
    }

    /// The prompt an agent's turn starts with.
    fn input(&self, current: usize, task: &str, message: Option<&str>, trail: &Trail) -> String {
        let members = &self.inner.members;
        let mut text = String::new();
        if let (Some(message), Some(last)) = (message, trail.handoffs.last()) {
            let _ = write!(text, "Handoff from {}: {message}\n\n", last.from);
        }
        let _ = write!(text, "User request: {task}\n\n");
        let before = &trail.agents[..trail.agents.len() - 1];
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
            .enumerate()
            .filter(|(index, _)| *index != current)
            .map(|(_, member)| member)
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
        text.trim_end().to_owned()
    }
}

impl Node for Swarm {
    fn run(&self, input: String) -> BoxFuture<'_, Result<NodeOutput, NodeError>> {
        Box::pin(async move {
            let run = Swarm::run(self, input).await?;
            Ok(NodeOutput::new(run.answer).with_usage(run.usage))
        })
    }
}

/// One agent's turn, as the handoff tool and hook see it.
struct Turn {
    agent: String,
    others: Vec<String>,
    handoff: Mutex<Option<Handoff>>,
}

impl Turn {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Handoff>> {
        self.handoff.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> Option<Handoff> {
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
         ends when you call this."
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
        if !turn.others.contains(&args.agent_name) {
            return Err(HandoffError::Unknown(
                args.agent_name,
                turn.others.join(", "),
            ));
        }
        let mut slot = turn.slot();
        if let Some(earlier) = &*slot {
            return Err(HandoffError::Twice(earlier.to.clone()));
        }
        let reply = format!("Handing off to {}.", args.agent_name);
        *slot = Some(Handoff {
            from: turn.agent.clone(),
            to: args.agent_name,
            message: args.message,
            context: args.context.filter(|context| !context.is_null()),
        });
        Ok(reply)
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
