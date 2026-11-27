//! Scoring a run.

use rig_agent::completion::StructuredOutputError;
use rig_core::completion::Usage;
use rig_core::error::BoxError;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use serde::{Deserialize, Serialize};

use crate::{Case, Trial};

/// One evaluator's verdict on one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Score {
    /// The evaluator's name. An experiment sets it to
    /// [`Evaluator::name`].
    pub evaluator: String,
    /// From 0 (worst) to 1 (best).
    pub value: f64,
    /// Whether the run passes this evaluator.
    pub passed: bool,
    /// A category, such as a judge's grade; `"error"` when the evaluator
    /// itself failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Why, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Whether the evaluator could not apply to this run, such as a
    /// reference the case lacks. Such a score is shown as N/A and left out
    /// of pass rates and means; `passed` is false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_applicable: bool,
    /// The model tokens the evaluator spent, when it called a model (an
    /// [`LlmJudge`](crate::LlmJudge)'s). The report adds them to its totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Score {
    /// A score of `value`, clamped to 0..=1 (`NaN` is 0).
    pub fn new(evaluator: impl Into<String>, value: f64, passed: bool) -> Self {
        Self {
            evaluator: evaluator.into(),
            value: clamp(value),
            passed,
            label: None,
            explanation: None,
            not_applicable: false,
            usage: None,
        }
    }

    /// Not applicable to the run, with why: shown as N/A and left out of
    /// pass rates and means.
    pub fn not_applicable(evaluator: impl Into<String>, explanation: impl Into<String>) -> Self {
        Self {
            not_applicable: true,
            ..Self::new(evaluator, 0.0, false)
                .with_label("n/a")
                .with_explanation(explanation)
        }
    }

    /// A pass: 1.
    pub fn pass(evaluator: impl Into<String>) -> Self {
        Self::new(evaluator, 1.0, true)
    }

    /// A failure: 0, with why.
    pub fn fail(evaluator: impl Into<String>, explanation: impl Into<String>) -> Self {
        Self::new(evaluator, 0.0, false).with_explanation(explanation)
    }

    /// Set the label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the explanation.
    #[must_use]
    pub fn with_explanation(mut self, explanation: impl Into<String>) -> Self {
        self.explanation = Some(explanation.into());
        self
    }

    /// Record the model tokens the evaluation spent.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}

/// `value` within 0..=1; `NaN` is 0.
pub(crate) fn clamp(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// Why an evaluator could not score a run. An experiment records it as a
/// failed score labelled `"error"` and carries on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// The case lacks the reference the evaluator compares with.
    #[error("case {case:?} has no {field} to compare with")]
    MissingReference {
        /// The case.
        case: String,
        /// The missing field: `expected` or `trajectory`.
        field: &'static str,
    },
    /// The judge model did not return a verdict. `LlmJudge` reports this as
    /// a failed score labelled `error` (keeping the tokens it spent) instead.
    #[error("the judge returned no verdict: {0}")]
    Judge(#[from] StructuredOutputError),
    /// Any other failure, such as a remote evaluator's.
    #[error(transparent)]
    Other(BoxError),
}

/// Scores runs.
pub trait Evaluator: WasmCompatSend + WasmCompatSync {
    /// Names the evaluator in scores, summaries and thresholds; unique
    /// within an experiment.
    fn name(&self) -> &str;

    /// Score `trial`, a run of `case`. A trial whose target failed has an
    /// [`error`](Trial::error); evaluators should fail it.
    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>>;

    /// This evaluator under another name: to use two of a kind in one
    /// experiment.
    fn named(self, name: impl Into<String>) -> Named<Self>
    where
        Self: Sized,
    {
        Named {
            name: name.into(),
            inner: self,
        }
    }
}

/// An evaluator renamed by [`Evaluator::named`].
pub struct Named<E> {
    name: String,
    inner: E,
}

impl<E: Evaluator> Evaluator for Named<E> {
    fn name(&self) -> &str {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        self.inner.evaluate(case, trial)
    }
}
