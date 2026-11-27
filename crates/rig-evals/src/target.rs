//! What an experiment evaluates: Rig agents, async functions and, with the
//! `multiagent` feature, swarms and graphs.

use std::future::Future;
use std::time::Duration;

use rig_agent::Agent;
use rig_core::completion::Usage;
use rig_core::message::{Message, ToolResultContent, UserContent};
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Case;

/// Why a target run failed: a boxed error, `Send + Sync` except on wasm.
pub type TargetError = rig_core::error::BoxError;

/// When a trial ends and the limit it was set from, for the target run of an
/// experiment with a [`trial_timeout`](crate::Experiment::trial_timeout).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    pub(crate) at: tokio::time::Instant,
    pub(crate) limit: Duration,
}

tokio::task_local! {
    /// The deadline of the trial whose target run this is. An agent target
    /// stops each turn at it, so a timed-out run keeps its completed turns.
    pub(crate) static TRIAL_DEADLINE: Deadline;
}

/// The explanation of a call that outlived `limit`.
pub(crate) fn timeout_message(limit: Duration) -> String {
    format!("timed out after {}s", limit.as_secs_f64())
}

/// The longest tool result kept, in bytes; a longer one is cut at a char
/// boundary and marked `[truncated]`.
const MAX_RESULT_BYTES: usize = 4096;

/// A tool call the target made, and the result the tool returned when the
/// target recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCall {
    /// The tool's name.
    pub name: String,
    /// The arguments; a string when the model sent arguments that were not a
    /// JSON object.
    pub args: Value,
    /// What the tool returned: its text parts (JSON as its JSON text, images
    /// as a placeholder) joined by newlines, cut to 4 KiB. `None` when the
    /// target did not record a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl ToolCall {
    /// A call to `name` with `args`, with no result.
    pub fn new(name: impl Into<String>, args: Value) -> Self {
        Self {
            name: name.into(),
            args,
            result: None,
        }
    }

    /// Record the result the tool returned.
    #[must_use]
    pub fn with_result(mut self, result: impl Into<String>) -> Self {
        self.result = Some(result.into());
        self
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

/// The tool calls of one run's messages, in order, each with the result its
/// tool returned. A result is matched to its call by id. A call with no
/// provider id (rig issued a local one) takes the next unmatched result whose
/// id belongs to no call of the run, so it never takes a later call's result.
fn tool_calls_of(messages: &[Message]) -> Vec<ToolCall> {
    let mut results: Vec<(String, Option<String>)> = messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match part {
            UserContent::ToolResult(result) => Some((
                result.call.wire().into_owned(),
                Some(render_result(&result.content)),
            )),
            _ => None,
        })
        .collect();
    let calls: Vec<&rig_core::message::ToolCall> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(turn) => Some(turn),
            _ => None,
        })
        .flat_map(|turn| turn.tool_calls())
        .collect();
    let call_ids: Vec<String> = calls
        .iter()
        .map(|call| call.id.wire().into_owned())
        .collect();
    calls
        .into_iter()
        .map(|call| {
            let id = call.id.wire();
            let slot = results
                .iter()
                .position(|(result_id, text)| text.is_some() && result_id == id.as_ref())
                .or_else(|| {
                    call.id.is_local().then(|| {
                        results.iter().position(|(result_id, text)| {
                            text.is_some() && !call_ids.contains(result_id)
                        })
                    })?
                });
            let tool = ToolCall::from(call);
            let text =
                slot.and_then(|slot| results.get_mut(slot).and_then(|(_, text)| text.take()));
            match text {
                Some(text) => tool.with_result(text),
                None => tool,
            }
        })
        .collect()
}

/// The text of a tool result's parts, joined by newlines and cut to
/// [`MAX_RESULT_BYTES`].
fn render_result(content: &[ToolResultContent]) -> String {
    let text = content
        .iter()
        .map(|part| match part {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Json { value } => value.to_string(),
            ToolResultContent::Image(_) => "[image]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() <= MAX_RESULT_BYTES {
        return text;
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = text;
    cut.truncate(end);
    cut.push_str(" [truncated]");
    cut
}

/// One user turn of a multi-turn run and the answer it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Turn {
    /// The user's message.
    pub input: String,
    /// The target's answer to it.
    pub output: String,
}

impl Turn {
    /// A turn of `input` answered by `output`.
    pub fn new(input: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
        }
    }
}

/// What one target run produced.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct TargetOutput {
    /// The answer: to the last turn of a multi-turn case.
    pub output: String,
    /// The tool calls, in order, over all turns.
    pub tool_calls: Vec<ToolCall>,
    /// The model tokens spent, over all turns.
    pub usage: Usage,
    /// The session the run belongs to, for evaluators that read its traces.
    pub session_id: Option<String>,
    /// Each turn and its answer, in order; empty for a single-turn run.
    pub transcript: Vec<Turn>,
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

    /// Record the turns of a multi-turn run.
    #[must_use]
    pub fn with_transcript(mut self, transcript: Vec<Turn>) -> Self {
        self.transcript = transcript;
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
    /// Prompts the agent with the case's input, or with each of its turns in
    /// order on one conversation (the agent keeps the history between
    /// turns). The tool calls are the ones in the runs' transcripts, each with
    /// the result its tool returned; the usage is the total over the turns. A multi-turn case that fails part
    /// way is a [`TurnFailed`] holding the turns that completed.
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>> {
        Box::pin(async move {
            let multi = !case.turns.is_empty();
            let deadline = TRIAL_DEADLINE.try_with(|deadline| *deadline).ok();
            // A multi-turn case carries its own history, starting empty, so
            // it does not pick up the agent's memory store; a single-turn
            // case runs as before.
            let mut history: Vec<Message> = Vec::new();
            let mut output = TargetOutput::default();
            for (number, input) in case.prompts().into_iter().enumerate() {
                let mut run = self.prompt(input.to_owned());
                if multi {
                    run = run.history(history.iter().cloned());
                }
                // Under a trial deadline the turn stops at it, while the
                // completed turns are still in hand.
                let outcome: Result<_, TargetError> = match deadline {
                    Some(deadline) => match tokio::time::timeout_at(deadline.at, run.run()).await {
                        Ok(result) => result.map_err(Into::into),
                        Err(_) => Err(timeout_message(deadline.limit).into()),
                    },
                    None => run.run().await.map_err(Into::into),
                };
                let response = match outcome {
                    Ok(response) => response,
                    Err(error) => return Err(turn_error(multi, number + 1, output, error)),
                };
                output.tool_calls.extend(tool_calls_of(&response.messages));
                output.usage += response.usage;
                let answer = response.output();
                output.transcript.push(Turn::new(input, answer.clone()));
                output.output = answer;
                history.extend(response.messages);
            }
            if !multi {
                output.transcript.clear();
            }
            Ok(output)
        })
    }
}

/// A multi-turn run that failed at one of its turns. The error of the turn
/// is prefixed `turn <n>:`; the completed turns, their tool calls and usage
/// (and the session id, if any) are kept in [`partial`](Self::partial), and
/// the experiment puts them in the failed trial.
#[derive(Debug)]
#[non_exhaustive]
pub struct TurnFailed {
    /// The turn that failed, from 1.
    pub turn: usize,
    /// What the run had before it failed.
    pub partial: TargetOutput,
    cause: TargetError,
}

impl TurnFailed {
    /// Turn `turn` failed with `cause`, after the turns in `partial`.
    pub(crate) fn new(turn: usize, partial: TargetOutput, cause: TargetError) -> Self {
        Self {
            turn,
            partial,
            cause,
        }
    }

    /// The error of the turn.
    pub fn cause(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.cause.as_ref()
    }
}

impl std::fmt::Display for TurnFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "turn {}: {}", self.turn, self.cause)
    }
}

impl std::error::Error for TurnFailed {}

/// The error of turn `turn` of a run, with what the run had before it. A
/// single-turn run keeps its error as it is.
pub(crate) fn turn_error(
    multi: bool,
    turn: usize,
    partial: TargetOutput,
    cause: TargetError,
) -> TargetError {
    if multi {
        Box::new(TurnFailed::new(turn, partial, cause))
    } else {
        cause
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
            let turns = case.prompts();
            let [prompt] = turns.as_slice() else {
                return Err(
                    "a node target runs single-turn cases only; its nodes keep no conversation"
                        .into(),
                );
            };
            let output = self.0.run((*prompt).to_owned(), &io).await?;
            Ok(TargetOutput::new(output.text).with_usage(output.usage))
        })
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::{AssistantContent, Text};
    use rig_core::message::{
        AssistantMessage, CallId, LocalCallId, ToolFunction, ToolName, ToolResultContent,
        UserContent,
    };

    use super::*;

    #[test]
    fn a_local_id_call_does_not_take_a_later_provider_call_result() {
        let call = |id: CallId, name: &str| {
            AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                id,
                ToolFunction::new(ToolName::new(name).expect("name"), serde_json::json!({})),
            ))
        };
        let messages = [
            Message::Assistant(AssistantMessage::new(vec![
                call(CallId::Local(LocalCallId::new()), "first"),
                call(CallId::from_wire("p2"), "second"),
            ])),
            Message::User {
                content: vec![UserContent::tool_result(
                    CallId::from_wire("p2"),
                    ToolName::new("second").expect("name"),
                    vec![ToolResultContent::Text(Text::new("B"))],
                )],
            },
        ];
        let results: Vec<Option<String>> = tool_calls_of(&messages)
            .into_iter()
            .map(|call| call.result)
            .collect();
        assert_eq!(results, [None, Some("B".to_owned())]);
    }
}
