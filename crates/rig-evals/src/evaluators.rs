//! The built-in evaluators.
//!
//! | Evaluator | Name | Passes when |
//! |---|---|---|
//! | [`contains`], [`contains_all`] | `contains` | the answer contains every string |
//! | [`regex()`] | `regex` | the answer matches the pattern |
//! | [`equals`] | `equals` | the trimmed answer equals the case's `expected` |
//! | [`json`] | `json` | the answer is JSON, with the required keys |
//! | [`trajectory`] | `trajectory` | the tool calls match the case's `trajectory` |
//! | [`budget`] | `budget` | tokens and latency are within limits |
//! | [`no_error`] | `no_error` | the target did not fail |
//! | [`LlmJudge`] | `llm_judge` | a judge model scores the answer at least the threshold |
//!
//! All of them fail a trial whose target failed.
//! [`named`](crate::Evaluator::named) renames one, to use two of a kind.

use std::time::Duration;

use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

use crate::{Case, EvalError, Evaluator, ExpectedCall, Score, ToolCall, Trial};

mod judge;

pub use judge::{LlmJudge, Verdict};

/// A failed score when the target failed.
pub(crate) fn target_failed(name: &str, trial: &Trial) -> Option<Score> {
    trial
        .error
        .as_ref()
        .map(|error| Score::fail(name, format!("the target failed: {error}")))
}

/// An evaluator whose check is synchronous.
fn ready<'a>(score: Result<Score, EvalError>) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
    Box::pin(std::future::ready(score))
}

/// `found` of `total`, as a value from 0 to 1; 1 when there is nothing to
/// find.
fn ratio(found: usize, total: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        found as f64 / total as f64
    }
}

/// See [`contains`].
#[derive(Debug, Clone)]
pub struct Contains {
    needles: Vec<String>,
    ignore_case: bool,
}

/// Passes when the answer contains `needle`.
pub fn contains(needle: impl Into<String>) -> Contains {
    contains_all([needle])
}

/// Passes when the answer contains every one of `needles`; the value is the
/// share found.
pub fn contains_all(needles: impl IntoIterator<Item = impl Into<String>>) -> Contains {
    Contains {
        needles: needles.into_iter().map(Into::into).collect(),
        ignore_case: false,
    }
}

impl Contains {
    /// Ignore case.
    #[must_use]
    pub fn ignore_case(mut self) -> Self {
        self.ignore_case = true;
        self
    }
}

impl Evaluator for Contains {
    fn name(&self) -> &str {
        "contains"
    }

    fn evaluate<'a>(
        &'a self,
        _case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let fold = |text: &str| {
            if self.ignore_case {
                text.to_lowercase()
            } else {
                text.to_owned()
            }
        };
        let output = fold(&trial.output);
        let missing: Vec<&str> = self
            .needles
            .iter()
            .filter(|needle| !output.contains(&fold(needle)))
            .map(String::as_str)
            .collect();
        let found = self.needles.len() - missing.len();
        let score = Score::new(
            self.name(),
            ratio(found, self.needles.len()),
            missing.is_empty(),
        );
        ready(Ok(if missing.is_empty() {
            score
        } else {
            score.with_explanation(format!("missing {missing:?}"))
        }))
    }
}

/// See [`regex()`].
#[derive(Debug, Clone)]
pub struct Matches(regex::Regex);

/// Passes when the answer matches `pattern` ([`regex`](https://docs.rs/regex)
/// syntax; `(?i)` ignores case).
pub fn regex(pattern: &str) -> Result<Matches, regex::Error> {
    regex::Regex::new(pattern).map(Matches)
}

impl Evaluator for Matches {
    fn name(&self) -> &str {
        "regex"
    }

    fn evaluate<'a>(
        &'a self,
        _case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        ready(Ok(if self.0.is_match(&trial.output) {
            Score::pass(self.name())
        } else {
            Score::fail(self.name(), format!("does not match {}", self.0.as_str()))
        }))
    }
}

/// See [`equals`].
#[derive(Debug, Clone, Default)]
pub struct Equals {
    ignore_case: bool,
    skip_without_reference: bool,
}

/// Passes when the answer, trimmed, equals the case's `expected`, trimmed.
/// A case without `expected` is an [`EvalError::MissingReference`].
pub fn equals() -> Equals {
    Equals::default()
}

impl Equals {
    /// Ignore case.
    #[must_use]
    pub fn ignore_case(mut self) -> Self {
        self.ignore_case = true;
        self
    }

    /// Score a case without `expected` as not applicable (N/A) instead of
    /// an error: for datasets that mix cases with and without an expected
    /// answer.
    #[must_use]
    pub fn not_applicable_without_reference(mut self) -> Self {
        self.skip_without_reference = true;
        self
    }
}

impl Evaluator for Equals {
    fn name(&self) -> &str {
        "equals"
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let Some(expected) = &case.expected else {
            if self.skip_without_reference {
                return ready(Ok(Score::not_applicable(
                    self.name(),
                    "the case has no expected answer",
                )));
            }
            return ready(Err(EvalError::MissingReference {
                case: case.id.clone(),
                field: "expected",
            }));
        };
        let (expected, output) = (expected.trim(), trial.output.trim());
        let equal = if self.ignore_case {
            expected.to_lowercase() == output.to_lowercase()
        } else {
            expected == output
        };
        ready(Ok(if equal {
            Score::pass(self.name())
        } else {
            Score::fail(
                self.name(),
                format!("expected {expected:?}, got {output:?}"),
            )
        }))
    }
}

/// See [`json`].
#[derive(Debug, Clone, Default)]
pub struct Json {
    required: Vec<String>,
}

/// Passes when the answer, trimmed, is JSON and, with
/// [`require`](Json::require), an object with the required keys.
pub fn json() -> Json {
    Json::default()
}

impl Json {
    /// Require the answer to be an object with `key`.
    #[must_use]
    pub fn require(mut self, key: impl Into<String>) -> Self {
        self.required.push(key.into());
        self
    }
}

impl Evaluator for Json {
    fn name(&self) -> &str {
        "json"
    }

    fn evaluate<'a>(
        &'a self,
        _case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let value: Value = match serde_json::from_str(trial.output.trim()) {
            Ok(value) => value,
            Err(error) => return ready(Ok(Score::fail(self.name(), format!("not JSON: {error}")))),
        };
        if self.required.is_empty() {
            return ready(Ok(Score::pass(self.name())));
        }
        let Some(object) = value.as_object() else {
            return ready(Ok(Score::fail(self.name(), "not a JSON object")));
        };
        let missing: Vec<&str> = self
            .required
            .iter()
            .filter(|key| !object.contains_key(key.as_str()))
            .map(String::as_str)
            .collect();
        ready(Ok(if missing.is_empty() {
            Score::pass(self.name())
        } else {
            Score::fail(self.name(), format!("missing keys {missing:?}"))
        }))
    }
}

/// How [`trajectory`] compares the tool calls with the expected ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrajectoryMode {
    /// The same calls in the same order, and no others.
    Exact,
    /// The expected calls in order; other calls may come between them.
    InOrder,
    /// The expected calls in any order; other calls may be made too.
    AnyOrder,
}

/// See [`trajectory`].
#[derive(Debug, Clone)]
pub struct Trajectory {
    mode: TrajectoryMode,
    skip_without_reference: bool,
}

/// Passes when the tool calls match the case's `trajectory` in `mode`. The
/// value is the share of expected calls matched. A case without a
/// trajectory is an [`EvalError::MissingReference`], unless
/// [`not_applicable_without_reference`](Trajectory::not_applicable_without_reference)
/// is set, when it is not applicable (N/A).
pub fn trajectory(mode: TrajectoryMode) -> Trajectory {
    Trajectory {
        mode,
        skip_without_reference: false,
    }
}

impl Trajectory {
    /// Score a case without a `trajectory` as not applicable (N/A) instead
    /// of an error: for datasets that mix cases with and without one.
    #[must_use]
    pub fn not_applicable_without_reference(mut self) -> Self {
        self.skip_without_reference = true;
        self
    }
}

/// Whether `actual` is a call `expected` describes.
fn call_matches(expected: &ExpectedCall, actual: &ToolCall) -> bool {
    expected.name == actual.name
        && expected
            .args
            .as_ref()
            .is_none_or(|args| match (args, &actual.args) {
                (Value::Object(want), Value::Object(got)) => {
                    want.iter().all(|(key, value)| got.get(key) == Some(value))
                }
                (want, got) => want == got,
            })
}

/// How many expected calls `actual` matches in `mode`, and the count that
/// means a full match.
fn matched(mode: TrajectoryMode, expected: &[ExpectedCall], actual: &[ToolCall]) -> (usize, usize) {
    match mode {
        TrajectoryMode::Exact => {
            let pairs = expected
                .iter()
                .zip(actual)
                .filter(|(want, got)| call_matches(want, got))
                .count();
            (pairs, expected.len().max(actual.len()))
        }
        TrajectoryMode::InOrder => {
            let mut wanted = expected.iter().peekable();
            for call in actual {
                wanted.next_if(|want| call_matches(want, call));
            }
            (expected.len() - wanted.count(), expected.len())
        }
        TrajectoryMode::AnyOrder => {
            // Calls with arguments first: a call without takes any leftover.
            let mut free: Vec<&ToolCall> = actual.iter().collect();
            let mut found = 0;
            let specific = expected.iter().filter(|call| call.args.is_some());
            let any = expected.iter().filter(|call| call.args.is_none());
            for want in specific.chain(any) {
                if let Some(at) = free.iter().position(|got| call_matches(want, got)) {
                    free.swap_remove(at);
                    found += 1;
                }
            }
            (found, expected.len())
        }
    }
}

impl Evaluator for Trajectory {
    fn name(&self) -> &str {
        "trajectory"
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let Some(expected) = &case.trajectory else {
            if self.skip_without_reference {
                return ready(Ok(Score::not_applicable(
                    self.name(),
                    "the case has no trajectory",
                )));
            }
            return ready(Err(EvalError::MissingReference {
                case: case.id.clone(),
                field: "trajectory",
            }));
        };
        let (found, total) = matched(self.mode, expected, &trial.tool_calls);
        let passed = found == total;
        let score = Score::new(self.name(), ratio(found, total), passed);
        if passed {
            return ready(Ok(score));
        }
        let want: Vec<&str> = expected.iter().map(|call| call.name.as_str()).collect();
        let got: Vec<&str> = trial
            .tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        ready(Ok(score.with_explanation(format!(
            "expected {want:?} ({:?}), got {got:?}",
            self.mode
        ))))
    }
}

/// See [`budget`].
#[derive(Debug, Clone, Default)]
pub struct Budget {
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    latency: Option<Duration>,
}

/// Passes when the run stays within the limits set. A token limit fails a
/// run whose model did not report that count.
pub fn budget() -> Budget {
    Budget::default()
}

impl Budget {
    /// At most `tokens` output tokens.
    #[must_use]
    pub fn max_output_tokens(mut self, tokens: u64) -> Self {
        self.output_tokens = Some(tokens);
        self
    }

    /// At most `tokens` tokens in all: the reported total, or input plus
    /// output.
    #[must_use]
    pub fn max_total_tokens(mut self, tokens: u64) -> Self {
        self.total_tokens = Some(tokens);
        self
    }

    /// At most `latency` for the run.
    #[must_use]
    pub fn max_latency(mut self, latency: Duration) -> Self {
        self.latency = Some(latency);
        self
    }
}

impl Evaluator for Budget {
    fn name(&self) -> &str {
        "budget"
    }

    fn evaluate<'a>(
        &'a self,
        _case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        if let Some(failed) = target_failed(self.name(), trial) {
            return ready(Ok(failed));
        }
        let usage = &trial.usage;
        let total = usage
            .total_tokens
            .or(match (usage.input_tokens, usage.output_tokens) {
                (None, None) => None,
                (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
            });
        let mut over = Vec::new();
        for (what, limit, used) in [
            ("output tokens", self.output_tokens, usage.output_tokens),
            ("total tokens", self.total_tokens, total),
        ] {
            match (limit, used) {
                (Some(limit), Some(used)) if used > limit => {
                    over.push(format!("{what}: {used} > {limit}"));
                }
                (Some(_), None) => over.push(format!("{what}: not reported")),
                _ => {}
            }
        }
        if let Some(limit) = self.latency
            && trial.latency > limit
        {
            over.push(format!("latency: {:?} > {limit:?}", trial.latency));
        }
        ready(Ok(if over.is_empty() {
            Score::pass(self.name())
        } else {
            Score::fail(self.name(), over.join("; "))
        }))
    }
}

/// See [`no_error`].
#[derive(Debug, Clone, Default)]
pub struct NoError;

/// Passes when the target did not fail.
pub fn no_error() -> NoError {
    NoError
}

impl Evaluator for NoError {
    fn name(&self) -> &str {
        "no_error"
    }

    fn evaluate<'a>(
        &'a self,
        _case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        ready(Ok(
            target_failed(self.name(), trial).unwrap_or_else(|| Score::pass(self.name()))
        ))
    }
}
