//! A model as the evaluator.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rig_agent::Agent;
use rig_agent::agent::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::{FinishReason, Usage};
use rig_core::wasm_compat::WasmBoxedFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ready, target_failed};
use crate::{Case, EvalError, Evaluator, Score, Trial};

/// What the judge model returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verdict {
    /// From 0 (fails the rubric) to 1 (fully meets it).
    pub score: f64,
    /// Why.
    pub reasoning: String,
}

/// A judge model scores the answer against a rubric: named `llm_judge`, or
/// by a preset ([`helpfulness`](Self::helpfulness), [`faithfulness`](Self::faithfulness),
/// [`goal_success`](Self::goal_success)).
///
/// The judge sees the rubric, the case's input, expected answer and
/// assertions (when it has them), the tool calls and the answer, and returns
/// a [`Verdict`] as structured output. The score is clamped to 0..=1 and
/// passes at the threshold (0.5 unless set); the reasoning is the
/// explanation.
pub struct LlmJudge {
    agent: Agent,
    name: &'static str,
    rubric: String,
    threshold: f64,
}

/// The rubric of [`LlmJudge::helpfulness`].
const HELPFULNESS: &str = "\
Score how helpful the answer is to the user's input, from 0 to 1.
- 1: directly and fully does what the user asked, is accurate as far as you can tell, and is clear enough to act on.
- 0.5: does part of what was asked, or is correct but vague, padded or missing a step the user needs.
- 0: ignores the request, is wrong, or is empty. An empty or refused answer scores 0 unless refusing is correct for the input.
Do not reward length or tone on their own.";

/// The rubric of [`LlmJudge::faithfulness`].
const FAITHFULNESS: &str = "\
Score whether the answer is faithful to the evidence, from 0 to 1. The evidence is the user's input, the transcript, the tool calls the agent made, the expected answer and the assertions. Tool outputs are not shown, so do not mark a claim unsupported only because its source is not shown.
- 1: every factual claim in the answer is supported by the evidence.
- 0.5: most claims are supported, but one claim is unsupported or only partly supported.
- 0: the answer contradicts the evidence, or most of its factual claims are unsupported by it.
Do not penalize an answer for leaving things out. An empty or refused answer scores 0 unless refusing is correct for the input.";

/// The rubric of [`LlmJudge::goal_success`].
const GOAL_SUCCESS: &str = "\
Score whether the agent achieved the user's goal across the whole conversation, from 0 to 1. The input turns are the user's messages in order, and the transcript holds each turn with the agent's answer. Judge the state at the end of the conversation, using every turn: a goal met early and undone later is not met.
- 1: by the last turn the user's goal is met, without the user having to repeat or correct themselves.
- 0.5: the goal is partly met, or met only after the user corrected the agent or asked again.
- 0: the goal is not met, the agent went off track or gave up, or the answer is empty or refused when refusing is wrong.
A single-turn case is judged the same way, on its one turn.";

impl LlmJudge {
    /// A judge running `agent` (the judge model) with `rubric`, named
    /// `llm_judge`.
    pub fn new(agent: Agent, rubric: impl Into<String>) -> Self {
        Self::named("llm_judge", agent, rubric)
    }

    fn named(name: &'static str, agent: Agent, rubric: impl Into<String>) -> Self {
        Self {
            agent,
            name,
            rubric: rubric.into(),
            threshold: 0.5,
        }
    }

    /// A preset judge named `helpfulness`: whether the answer directly and
    /// fully does what the user asked, from 0 to 1. Passes at 0.5 unless
    /// [`threshold`](Self::threshold) sets otherwise.
    pub fn helpfulness(agent: Agent) -> Self {
        Self::named("helpfulness", agent, HELPFULNESS)
    }

    /// A preset judge named `faithfulness`: whether the answer states only
    /// what the transcript, the tool calls and the case's references support,
    /// with no invented facts. Tool outputs are not in a trial, so the judge
    /// sees the tool calls but not their results. Passes at 0.5 unless set.
    pub fn faithfulness(agent: Agent) -> Self {
        Self::named("faithfulness", agent, FAITHFULNESS)
    }

    /// A preset judge named `goal_success`: whether, by the end of the case,
    /// the agent achieved the user's goal, reading every turn of a multi-turn
    /// case. Passes at 0.5 unless set.
    pub fn goal_success(agent: Agent) -> Self {
        Self::named("goal_success", agent, GOAL_SUCCESS)
    }

    /// Pass at `threshold` and above.
    #[must_use]
    pub fn threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// What the judge is asked.
    fn prompt(&self, case: &Case, trial: &Trial) -> String {
        let mut prompt = format!(
            "You grade the answer of an AI agent.\n\nRubric:\n{}\n\n",
            self.rubric
        );
        let prompts = case.prompts();
        if let [input] = prompts.as_slice() {
            let _ = write!(prompt, "Input:\n{input}\n");
        } else {
            prompt.push_str("Input turns, sent in order on one conversation:\n");
            for (number, input) in prompts.iter().enumerate() {
                let _ = writeln!(prompt, "{}. {input}", number + 1);
            }
        }
        if let Some(expected) = &case.expected {
            let _ = write!(prompt, "\nExpected answer:\n{expected}\n");
        }
        if !case.assertions.is_empty() {
            prompt.push_str("\nThe answer must satisfy:\n");
            for assertion in &case.assertions {
                let _ = writeln!(prompt, "- {assertion}");
            }
        }
        if !trial.transcript.is_empty() {
            prompt.push_str("\nTranscript:\n");
            for turn in &trial.transcript {
                let _ = write!(prompt, "User: {}\nAgent: {}\n", turn.input, turn.output);
            }
        }
        if !trial.tool_calls.is_empty() {
            prompt.push_str("\nTool calls the agent made:\n");
            for call in &trial.tool_calls {
                let _ = writeln!(prompt, "- {}({})", call.name, call.args);
            }
        }
        let _ = write!(
            prompt,
            "\nAnswer to grade:\n{}\n\nScore it from 0 (fails the rubric) to 1 (fully meets it), and give your reasoning.",
            trial.output
        );
        prompt
    }
}

/// What the judge's turns reported: whether the latest turn stopped at its
/// token cap (and the cap), and the usage of every turn.
#[derive(Debug, Clone, Default)]
struct Watch {
    /// `Some(cap)` when the last turn stopped for length, else `None`.
    cut_off: Option<Option<u64>>,
    /// Usage summed over every turn, including retried ones.
    usage: Usage,
}

impl Watch {
    fn observe(
        &mut self,
        finish_reason: Option<&FinishReason>,
        max_tokens: Option<u64>,
        usage: Usage,
    ) {
        self.cut_off = matches!(finish_reason, Some(FinishReason::Length)).then_some(max_tokens);
        self.usage += usage;
    }
}

/// Feeds each judge turn to a [`Watch`].
struct Observe(Arc<Mutex<Watch>>);

impl AgentHook for Observe {
    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if let Ok(mut watch) = self.0.lock() {
            watch.observe(event.finish_reason, event.max_tokens, event.usage);
        }
        ModelTurnAction::Continue
    }
}

impl Evaluator for LlmJudge {
    fn name(&self) -> &str {
        self.name
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let watch = Arc::new(Mutex::new(Watch::default()));
        let run = self
            .agent
            .prompt_typed::<Verdict>(self.prompt(case, trial))
            .add_hook(Observe(Arc::clone(&watch)));
        Box::pin(async move {
            match run.await {
                Ok(response) => {
                    let verdict = response.output;
                    let score = Score::new(self.name(), verdict.score, false);
                    let passed = score.value >= self.threshold;
                    Ok(Score { passed, ..score }
                        .with_explanation(verdict.reasoning)
                        .with_usage(response.usage))
                }
                Err(error) => {
                    // A failed judgment is a failed score labelled error, as an
                    // experiment records an evaluator error, but it keeps the
                    // tokens the judge spent.
                    let watch = watch.lock().map(|watch| watch.clone()).unwrap_or_default();
                    let message = match watch.cut_off {
                        Some(max_tokens) => format!(
                            "the judge returned no verdict: the reply was truncated at max_tokens{}",
                            max_tokens.map(|n| format!(" ({n})")).unwrap_or_default()
                        ),
                        None => format!("the judge returned no verdict: {error}"),
                    };
                    Ok(Score::fail(self.name(), message)
                        .with_label("error")
                        .with_usage(watch.usage))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cut_off_is_the_last_turns_and_the_usage_adds_up() {
        let usage = |n| {
            Usage::new()
                .input_tokens(n)
                .output_tokens(n)
                .total_tokens(2 * n)
        };
        let mut watch = Watch::default();
        watch.observe(Some(&FinishReason::Length), Some(64), usage(1));
        assert_eq!(watch.cut_off, Some(Some(64)));

        watch.observe(Some(&FinishReason::Stop), Some(64), usage(2));
        assert_eq!(watch.cut_off, None, "a later stop clears an earlier length");

        watch.observe(Some(&FinishReason::Length), None, usage(3));
        assert_eq!(watch.cut_off, Some(None), "a length without a cap");
        assert_eq!(watch.usage.total_tokens, Some(12));
    }
}
