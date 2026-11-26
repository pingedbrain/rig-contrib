//! What an experiment evaluates: Rig agents, async functions and, with the
//! `multiagent` feature, swarms and graphs.

use std::future::Future;

use rig_agent::Agent;
use rig_core::completion::Usage;
use rig_core::message::Message;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Case;

/// Why a target run failed: a boxed error, `Send + Sync` except on wasm.
pub type TargetError = rig_core::error::BoxError;

/// A tool call the target made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCall {
    /// The tool's name.
    pub name: String,
    /// The arguments; a string when the model sent arguments that were not a
    /// JSON object.
    pub args: Value,
}

impl ToolCall {
    /// A call to `name` with `args`.
    pub fn new(name: impl Into<String>, args: Value) -> Self {
        Self {
            name: name.into(),
            args,
        }
    }
}

impl From<&rig_core::message::ToolCall> for ToolCall {
    fn from(call: &rig_core::message::ToolCall) -> Self {
        let function = &call.function;
        let args = match &function.invalid_arguments {
            Some(raw) => Value::String(raw.clone()),
            None => Value::Object(function.arguments.clone()),
        };
        Self::new(function.name.as_str(), args)
    }
}

/// What one target run produced.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct TargetOutput {
    /// The answer.
    pub output: String,
    /// The tool calls, in order.
    pub tool_calls: Vec<ToolCall>,
    /// The model tokens spent.
    pub usage: Usage,
    /// The session the run belongs to, for evaluators that read its traces.
    pub session_id: Option<String>,
}

impl TargetOutput {
    /// An answer with no tool calls or usage.
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Self::default()
        }
    }

    /// Record the tool calls made.
    #[must_use]
    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCall>) -> Self {
        self.tool_calls = tool_calls;
        self
    }

    /// Record the model tokens spent.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Record the session the run belongs to.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }
}

impl From<String> for TargetOutput {
    fn from(output: String) -> Self {
        Self::new(output)
    }
}

impl From<&str> for TargetOutput {
    fn from(output: &str) -> Self {
        Self::new(output)
    }
}

/// Something to evaluate: runs a case's input and reports what it did.
pub trait Target: WasmCompatSend + WasmCompatSync {
    /// Run `case`.
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>>;
}

impl Target for Agent {
    /// Prompts the agent with the case's input. The tool calls are the ones
    /// in the run's transcript; the usage is the run's total.
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>> {
        Box::pin(async move {
            let response = self.prompt(case.input.clone()).run().await?;
            let tool_calls = response
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::Assistant(turn) => Some(turn),
                    _ => None,
                })
                .flat_map(|turn| turn.tool_calls().map(ToolCall::from))
                .collect();
            Ok(TargetOutput::new(response.output())
                .with_tool_calls(tool_calls)
                .with_usage(response.usage))
        })
    }
}

/// A target made of an async function; see [`target_fn`].
pub struct FnTarget<F>(F);

/// A target that runs `f` on each case: a wrapper around an agent pipeline,
/// a remote service, or a stub. `f` returns a [`TargetOutput`] or anything
/// that converts into one (a `String`).
///
/// ```
/// use rig_evals::{Case, TargetError, target_fn};
/// let echo = target_fn(|case: Case| async move { Ok::<_, TargetError>(case.input) });
/// ```
pub fn target_fn<F, Fut, O>(f: F) -> FnTarget<F>
where
    F: Fn(Case) -> Fut + WasmCompatSend + WasmCompatSync,
    Fut: Future<Output = Result<O, TargetError>> + WasmCompatSend + 'static,
    O: Into<TargetOutput>,
{
    FnTarget(f)
}

impl<F, Fut, O> Target for FnTarget<F>
where
    F: Fn(Case) -> Fut + WasmCompatSend + WasmCompatSync,
    Fut: Future<Output = Result<O, TargetError>> + WasmCompatSend + 'static,
    O: Into<TargetOutput>,
{
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>> {
        let future = (self.0)(case.clone());
        Box::pin(async move { future.await.map(Into::into) })
    }
}

/// A rig-multiagent node (a swarm, a graph, an agent) as a target; see
/// [`node_target`].
#[cfg(feature = "multiagent")]
pub struct NodeTarget<N>(N);

/// Evaluate a rig-multiagent [`Node`](rig_multiagent::Node): its answer and
/// usage. Nodes do not report their tool calls, so trials have none.
#[cfg(feature = "multiagent")]
pub fn node_target<N: rig_multiagent::Node>(node: N) -> NodeTarget<N> {
    NodeTarget(node)
}

#[cfg(feature = "multiagent")]
impl<N: rig_multiagent::Node> Target for NodeTarget<N> {
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>> {
        Box::pin(async move {
            let io = rig_multiagent::NodeIo::quiet(&case.id);
            let output = self.0.run(case.input.clone(), &io).await?;
            Ok(TargetOutput::new(output.text).with_usage(output.usage))
        })
    }
}
