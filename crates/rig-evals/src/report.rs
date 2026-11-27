//! What an experiment found, and its JSON, JUnit and Markdown forms.

use std::fmt::Write as _;
use std::time::Duration;

use rig_core::completion::Usage;
use serde::{Deserialize, Serialize};

use crate::{Score, ToolCall, Turn};

/// One run of the target on a case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Trial {
    /// The case's id.
    pub case_id: String,
    /// Which run of the case this is, from 1.
    pub repetition: usize,
    /// The answer; empty when the target failed.
    pub output: String,
    /// The tool calls, in order.
    pub tool_calls: Vec<ToolCall>,
    /// The model tokens spent.
    pub usage: Usage,
    /// How long the run took.
    #[serde(rename = "latency_ms", with = "millis")]
    pub latency: Duration,
    /// Why the target failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The session the target ran in, when it reports one (an AgentCore
    /// runtime does): remote evaluators look up the run's traces by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Each turn of a multi-turn case and its answer, in order; empty for a
    /// single-turn case. `output` is the answer to the last turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<Turn>,
}

impl Trial {
    /// A successful run of `case_id` that answered `output`, with no tool
    /// calls, usage or latency: to test an evaluator.
    pub fn new(case_id: impl Into<String>, repetition: usize, output: impl Into<String>) -> Self {
        Self {
            case_id: case_id.into(),
            repetition,
            output: output.into(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            latency: Duration::ZERO,
            error: None,
            session_id: None,
            transcript: Vec::new(),
        }
    }
}

/// A trial and its scores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TrialResult {
    /// The run.
    pub trial: Trial,
    /// One per evaluator, in the experiment's order.
    pub scores: Vec<Score>,
}

impl TrialResult {
    /// Whether every evaluator passed the run.
    pub fn passed(&self) -> bool {
        self.scores
            .iter()
            .all(|score| score.passed || score.not_applicable)
    }

    /// Whether an evaluator did not apply to the run: with no failure, the
    /// run is reported as skipped in JUnit.
    fn skipped(&self) -> bool {
        self.scores.iter().any(|score| score.not_applicable)
    }

    /// The scores that failed (not the ones not applicable).
    pub fn failures(&self) -> impl Iterator<Item = &Score> {
        self.scores
            .iter()
            .filter(|score| !score.passed && !score.not_applicable)
    }
}

/// One evaluator's results over all trials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvaluatorSummary {
    /// The evaluator.
    pub evaluator: String,
    /// The trials scored (not counting the ones not applicable).
    pub count: usize,
    /// The trials it passed.
    pub passed: usize,
    /// `passed / count`; 0 when no trial was applicable.
    pub pass_rate: f64,
    /// The mean score value over the trials scored.
    pub mean: f64,
    /// The trials the evaluator did not apply to (N/A): left out of
    /// `count`, `pass_rate` and `mean`.
    #[serde(default)]
    pub not_applicable: usize,
}

/// Model tokens over all trials.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Tokens {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// The reported totals, or input plus output where none was reported.
    pub total: u64,
}

/// Run latencies over all trials.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LatencySummary {
    /// The median.
    #[serde(rename = "p50_ms", with = "millis")]
    pub p50: Duration,
    /// The 95th percentile.
    #[serde(rename = "p95_ms", with = "millis")]
    pub p95: Duration,
}

/// Whether an evaluator's pass rate reached its threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ThresholdResult {
    /// The evaluator.
    pub evaluator: String,
    /// The lowest pass rate that passes: 1 unless the experiment set one.
    pub min_pass_rate: f64,
    /// The pass rate reached.
    pub pass_rate: f64,
    /// `pass_rate >= min_pass_rate`, or no trial was applicable.
    pub passed: bool,
}

/// What an experiment found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Report {
    /// The experiment's name.
    pub experiment: String,
    /// How many times each case ran.
    pub repetitions: usize,
    /// Every trial, by case then repetition.
    pub trials: Vec<TrialResult>,
    /// Per evaluator, in the experiment's order.
    pub summary: Vec<EvaluatorSummary>,
    /// Model tokens: the target's and the LLM judges'.
    pub tokens: Tokens,
    /// The part of `tokens` that LLM judges spent.
    #[serde(default)]
    pub judge_tokens: Tokens,
    /// Run latencies.
    pub latency: LatencySummary,
    /// One per evaluator.
    pub thresholds: Vec<ThresholdResult>,
}

impl Report {
    /// Summarize `trials`, scored by `evaluators` with minimum pass rates
    /// `thresholds` (1 for the others).
    pub(crate) fn new(
        experiment: String,
        repetitions: usize,
        trials: Vec<TrialResult>,
        evaluators: &[String],
        thresholds: &[(String, f64)],
    ) -> Self {
        let summary: Vec<EvaluatorSummary> = evaluators
            .iter()
            .map(|evaluator| {
                let scores = trials
                    .iter()
                    .flat_map(|trial| &trial.scores)
                    .filter(|score| &score.evaluator == evaluator);
                let (count, passed, sum, not_applicable) =
                    scores.fold((0, 0, 0.0, 0), |(count, passed, sum, na), score| {
                        if score.not_applicable {
                            return (count, passed, sum, na + 1);
                        }
                        (
                            count + 1,
                            passed + usize::from(score.passed),
                            sum + score.value,
                            na,
                        )
                    });
                let per = |value: f64| {
                    if count == 0 {
                        0.0
                    } else {
                        value / count as f64
                    }
                };
                EvaluatorSummary {
                    evaluator: evaluator.clone(),
                    count,
                    passed,
                    pass_rate: per(passed as f64),
                    mean: per(sum),
                    not_applicable,
                }
            })
            .collect();
        let thresholds = summary
            .iter()
            .map(|summary| {
                let min_pass_rate = thresholds
                    .iter()
                    .rev()
                    .find(|(name, _)| name == &summary.evaluator)
                    .map_or(1.0, |(_, min)| *min);
                ThresholdResult {
                    evaluator: summary.evaluator.clone(),
                    min_pass_rate,
                    pass_rate: summary.pass_rate,
                    // Nothing applicable to fail.
                    passed: summary.count == 0 || summary.pass_rate >= min_pass_rate,
                }
            })
            .collect();
        let mut tokens = Tokens::default();
        for usage in trials.iter().map(|trial| &trial.trial.usage) {
            add_usage(&mut tokens, usage);
        }
        let mut judge_tokens = Tokens::default();
        for usage in trials
            .iter()
            .flat_map(|trial| &trial.scores)
            .filter_map(|score| score.usage.as_ref())
        {
            add_usage(&mut judge_tokens, usage);
        }
        tokens.input += judge_tokens.input;
        tokens.output += judge_tokens.output;
        tokens.total += judge_tokens.total;
        let mut latencies: Vec<Duration> = trials.iter().map(|trial| trial.trial.latency).collect();
        latencies.sort_unstable();
        let latency = LatencySummary {
            p50: percentile(&latencies, 0.50),
            p95: percentile(&latencies, 0.95),
        };
        Self {
            experiment,
            repetitions,
            trials,
            summary,
            tokens,
            judge_tokens,
            latency,
            thresholds,
        }
    }

    /// Whether every evaluator reached its threshold: the exit status for CI.
    pub fn passed(&self) -> bool {
        self.thresholds.iter().all(|threshold| threshold.passed)
    }

    /// An evaluator's pass rate.
    pub fn pass_rate(&self, evaluator: &str) -> Option<f64> {
        self.summary
            .iter()
            .find(|summary| summary.evaluator == evaluator)
            .map(|summary| summary.pass_rate)
    }

    /// The report as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The report as JUnit XML: a test case per trial (failing when an
    /// evaluator failed it) and per threshold.
    pub fn to_junit(&self) -> String {
        let name = escape(&self.experiment);
        let tests = self.trials.len() + self.thresholds.len();
        let skipped = self.trials.iter().filter(|trial| trial.skipped()).count();
        let failures = self.trials.iter().filter(|trial| !trial.passed()).count()
            + self
                .thresholds
                .iter()
                .filter(|threshold| !threshold.passed)
                .count();
        let time: f64 = self
            .trials
            .iter()
            .map(|trial| trial.trial.latency.as_secs_f64())
            .sum();
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            xml,
            "<testsuites name=\"{name}\" tests=\"{tests}\" failures=\"{failures}\" time=\"{time:.3}\">"
        );
        let _ = writeln!(
            xml,
            "  <testsuite name=\"{name}\" tests=\"{tests}\" failures=\"{failures}\" errors=\"0\" skipped=\"{skipped}\" time=\"{time:.3}\">"
        );
        for result in &self.trials {
            let trial = &result.trial;
            let case = if self.repetitions > 1 {
                format!("{} #{}", trial.case_id, trial.repetition)
            } else {
                trial.case_id.clone()
            };
            let _ = write!(
                xml,
                "    <testcase name=\"{}\" classname=\"{name}\" time=\"{:.3}\"",
                escape(&case),
                trial.latency.as_secs_f64()
            );
            let failed: Vec<&Score> = result.failures().collect();
            if failed.is_empty() {
                if result.skipped() {
                    let why: Vec<String> = result
                        .scores
                        .iter()
                        .filter(|score| score.not_applicable)
                        .map(|score| {
                            format!(
                                "{}: {}",
                                score.evaluator,
                                score.explanation.as_deref().unwrap_or("not applicable")
                            )
                        })
                        .collect();
                    let message = format!("not applicable: {}", why.join("; "));
                    let _ = write!(
                        xml,
                        ">\n      <skipped message=\"{}\"/>\n    </testcase>\n",
                        escape(&message)
                    );
                } else {
                    xml.push_str("/>\n");
                }
                continue;
            }
            let names: Vec<&str> = failed
                .iter()
                .map(|score| score.evaluator.as_str())
                .collect();
            let mut body = String::new();
            if let Some(error) = &trial.error {
                let _ = writeln!(body, "target error: {error}");
            }
            for score in &failed {
                let _ = writeln!(
                    body,
                    "{} ({:.2}): {}",
                    score.evaluator,
                    score.value,
                    score.explanation.as_deref().unwrap_or("failed")
                );
            }
            let _ = write!(
                xml,
                ">\n      <failure message=\"failed: {}\" type=\"evaluation\">{}</failure>\n    </testcase>\n",
                escape(&names.join(", ")),
                escape(&body)
            );
        }
        for threshold in &self.thresholds {
            let _ = write!(
                xml,
                "    <testcase name=\"threshold {}\" classname=\"{name}.thresholds\"",
                escape(&threshold.evaluator)
            );
            if threshold.passed {
                xml.push_str("/>\n");
            } else {
                let _ = write!(
                    xml,
                    ">\n      <failure message=\"pass rate {:.2} below {:.2}\" type=\"threshold\"/>\n    </testcase>\n",
                    threshold.pass_rate, threshold.min_pass_rate
                );
            }
        }
        xml.push_str("  </testsuite>\n</testsuites>\n");
        xml
    }

    /// The report as Markdown: the outcome, a table per evaluator, tokens,
    /// latency, and the failed trials.
    pub fn to_markdown(&self) -> String {
        let outcome = if self.passed() { "passed" } else { "FAILED" };
        let mut md = format!(
            "# {}: {outcome}\n\n{} trials ({} cases × {} repetitions)\n\n",
            self.experiment,
            self.trials.len(),
            self.trials.len() / self.repetitions.max(1),
            self.repetitions
        );
        md.push_str("| Evaluator | Pass rate | Threshold | Mean | Passed | Result |\n|---|---|---|---|---|---|\n");
        for (summary, threshold) in self.summary.iter().zip(&self.thresholds) {
            let applicable = summary.count > 0;
            let (rate, mean) = if applicable {
                (
                    format!("{:.0}%", summary.pass_rate * 100.0),
                    format!("{:.2}", summary.mean),
                )
            } else {
                ("N/A".to_owned(), "N/A".to_owned())
            };
            let not_applicable = if summary.not_applicable > 0 {
                format!(" ({} N/A)", summary.not_applicable)
            } else {
                String::new()
            };
            let result = match (applicable, threshold.passed) {
                (false, _) => "N/A",
                (true, true) => "pass",
                (true, false) => "FAIL",
            };
            let _ = writeln!(
                md,
                "| {} | {rate} | {:.0}% | {mean} | {}/{}{not_applicable} | {result} |",
                cell(&summary.evaluator),
                threshold.min_pass_rate * 100.0,
                summary.passed,
                summary.count,
            );
        }
        let judges = if self.judge_tokens == Tokens::default() {
            String::new()
        } else {
            format!(
                " Of which LLM judges: {} input, {} output, {} total.",
                self.judge_tokens.input, self.judge_tokens.output, self.judge_tokens.total
            )
        };
        let _ = writeln!(
            md,
            "\nTokens: {} input, {} output, {} total.{judges} Latency: p50 {:?}, p95 {:?}.",
            self.tokens.input,
            self.tokens.output,
            self.tokens.total,
            self.latency.p50,
            self.latency.p95
        );
        let failed: Vec<&TrialResult> =
            self.trials.iter().filter(|trial| !trial.passed()).collect();
        if !failed.is_empty() {
            md.push_str("\n## Failures\n\n");
            for result in failed {
                let trial = &result.trial;
                for score in result.failures() {
                    let _ = writeln!(
                        md,
                        "- `{}` #{} {}: {}",
                        trial.case_id,
                        trial.repetition,
                        score.evaluator,
                        one_line(score.explanation.as_deref().unwrap_or("failed"))
                    );
                }
            }
        }
        md
    }
}

/// Add `usage` to `tokens`; a total not reported is input plus output.
fn add_usage(tokens: &mut Tokens, usage: &Usage) {
    let (input, output) = (
        usage.input_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
    );
    tokens.input += input;
    tokens.output += output;
    tokens.total += usage.total_tokens.unwrap_or(input + output);
}

/// The nearest-rank percentile `p` of sorted `values`; zero when empty.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or_default()
}

/// `text` safe in XML text and attributes: markup escaped, and the control
/// characters XML 1.0 forbids dropped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// `text` on one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` safe in a Markdown table cell.
fn cell(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

/// A duration as fractional milliseconds.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        duration: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(duration.as_secs_f64() * 1000.0)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        let millis = f64::deserialize(deserializer)?;
        Duration::try_from_secs_f64(millis / 1000.0).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
