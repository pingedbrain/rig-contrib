//! A model as the evaluator.

use std::fmt::Write as _;

use rig_agent::Agent;
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
        let verdict = self.agent.prompt_typed::<Verdict>(self.prompt(case, trial));
        Box::pin(async move {
            let verdict = verdict.await?.output;
            let score = Score::new(self.name(), verdict.score, false);
            let passed = score.value >= self.threshold;
            Ok(Score { passed, ..score }.with_explanation(verdict.reasoning))
        })
    }
}
