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

/// A judge model scores the answer against a rubric: named `llm_judge`.
///
/// The judge sees the rubric, the case's input, expected answer and
/// assertions (when it has them), the tool calls and the answer, and returns
/// a [`Verdict`] as structured output. The score is clamped to 0..=1 and
/// passes at the threshold (0.5 unless set); the reasoning is the
/// explanation.
pub struct LlmJudge {
    agent: Agent,
    rubric: String,
    threshold: f64,
}

impl LlmJudge {
    /// A judge running `agent` (the judge model) with `rubric`.
    pub fn new(agent: Agent, rubric: impl Into<String>) -> Self {
        Self {
            agent,
            rubric: rubric.into(),
            threshold: 0.5,
        }
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
            "You grade the answer of an AI agent.\n\nRubric:\n{}\n\nInput:\n{}\n",
            self.rubric, case.input
        );
        if let Some(expected) = &case.expected {
            let _ = write!(prompt, "\nExpected answer:\n{expected}\n");
        }
        if !case.assertions.is_empty() {
            prompt.push_str("\nThe answer must satisfy:\n");
            for assertion in &case.assertions {
                let _ = writeln!(prompt, "- {assertion}");
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
        "llm_judge"
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
