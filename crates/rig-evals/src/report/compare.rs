//! Comparing two reports of one experiment: a baseline run against a candidate
//! run, such as yesterday's against today's or model A against model B.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{EvaluatorSummary, LatencySummary, Report, Tokens, TrialResult, cell, one_line};

/// One evaluator in a [`Comparison`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvaluatorDelta {
    /// The evaluator.
    pub evaluator: String,
    /// Its summary in the baseline; `None` when the baseline has no such
    /// evaluator.
    pub baseline: Option<EvaluatorSummary>,
    /// Its summary in the candidate; `None` when the candidate has no such
    /// evaluator.
    pub candidate: Option<EvaluatorSummary>,
    /// `candidate.pass_rate - baseline.pass_rate`; `None` unless both reports
    /// have the evaluator with at least one scored trial (`count > 0`).
    pub pass_rate_delta: Option<f64>,
    /// `candidate.mean - baseline.mean`; `None` unless both reports have the
    /// evaluator with at least one scored trial (`count > 0`).
    pub mean_delta: Option<f64>,
}

/// A case whose outcome changed between the two reports.
///
/// A case passes in a report when every one of its trials passes. Trials that
/// share a case id are pooled into one case (see [`Report::compare`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CaseChange {
    /// The case's id.
    pub case_id: String,
    /// The evaluators that failed the case in the report where it failed: the
    /// candidate's for a regression, the baseline's for a fix.
    pub evaluators: Vec<String>,
}

/// How a candidate run differs from a baseline run of the same experiment.
///
/// Built by [`Report::compare`]. Cases are matched by id. Trials that share
/// an id are pooled into one case, in both reports, so a duplicated id counts
/// once in the regression, fix and only-in lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Comparison {
    /// The baseline's experiment name.
    pub baseline_experiment: String,
    /// The candidate's experiment name.
    pub candidate_experiment: String,
    /// Per evaluator: those of the baseline first, in its order, then those
    /// only the candidate has.
    pub evaluators: Vec<EvaluatorDelta>,
    /// Cases that passed in the baseline and fail in the candidate, by id.
    pub regressions: Vec<CaseChange>,
    /// Cases that failed in the baseline and pass in the candidate, by id.
    pub fixes: Vec<CaseChange>,
    /// Case ids only the baseline has, sorted.
    pub only_in_baseline: Vec<String>,
    /// Case ids only the candidate has, sorted.
    pub only_in_candidate: Vec<String>,
    /// The baseline's model tokens.
    pub baseline_tokens: Tokens,
    /// The candidate's model tokens.
    pub candidate_tokens: Tokens,
    /// `candidate_tokens.total - baseline_tokens.total`.
    pub tokens_delta: i64,
    /// The baseline's run latencies.
    pub baseline_latency: LatencySummary,
    /// The candidate's run latencies.
    pub candidate_latency: LatencySummary,
    /// The candidate's p50 minus the baseline's, in milliseconds.
    pub p50_delta_ms: f64,
    /// The candidate's p95 minus the baseline's, in milliseconds.
    pub p95_delta_ms: f64,
}

impl Report {
    /// Compares this report (the candidate) with `baseline`: per evaluator the
    /// pass-rate and mean deltas, the cases that regressed or were fixed, the
    /// cases only one report has, and the token and latency deltas.
    ///
    /// Cases are matched by id. Trials that share an id within a report are
    /// pooled into one case: it passes only when all of them pass, and its
    /// failing evaluators are those of all of them. Repetitions of a case
    /// therefore count as one case here, not as separate cases.
    ///
    /// An evaluator's deltas are `None` unless both reports score it in at
    /// least one trial.
    ///
    /// ```
    /// use rig_evals::Report;
    ///
    /// fn regressed(baseline: &Report, candidate: &Report) -> bool {
    ///     !candidate.compare(baseline).regressions.is_empty()
    /// }
    /// ```
    pub fn compare(&self, baseline: &Report) -> Comparison {
        let base_cases = cases(baseline);
        let cand_cases = cases(self);
        let mut regressions = Vec::new();
        let mut fixes = Vec::new();
        let mut only_in_baseline = Vec::new();
        for (id, base_trials) in &base_cases {
            let Some(cand_trials) = cand_cases.get(id) else {
                only_in_baseline.push((*id).to_owned());
                continue;
            };
            let base_passed = base_trials.iter().all(|trial| trial.passed());
            let cand_passed = cand_trials.iter().all(|trial| trial.passed());
            if base_passed && !cand_passed {
                regressions.push(CaseChange {
                    case_id: (*id).to_owned(),
                    evaluators: failing_evaluators(cand_trials),
                });
            } else if !base_passed && cand_passed {
                fixes.push(CaseChange {
                    case_id: (*id).to_owned(),
                    evaluators: failing_evaluators(base_trials),
                });
            }
        }
        let only_in_candidate = cand_cases
            .keys()
            .filter(|id| !base_cases.contains_key(*id))
            .map(|id| (*id).to_owned())
            .collect();

        let mut evaluators = Vec::new();
        for base in &baseline.summary {
            let cand = self
                .summary
                .iter()
                .find(|summary| summary.evaluator == base.evaluator);
            evaluators.push(evaluator_delta(&base.evaluator, Some(base), cand));
        }
        for cand in &self.summary {
            let in_baseline = baseline
                .summary
                .iter()
                .any(|summary| summary.evaluator == cand.evaluator);
            if !in_baseline {
                evaluators.push(evaluator_delta(&cand.evaluator, None, Some(cand)));
            }
        }

        Comparison {
            baseline_experiment: baseline.experiment.clone(),
            candidate_experiment: self.experiment.clone(),
            evaluators,
            regressions,
            fixes,
            only_in_baseline,
            only_in_candidate,
            baseline_tokens: baseline.tokens,
            candidate_tokens: self.tokens,
            tokens_delta: signed_delta(self.tokens.total, baseline.tokens.total),
            baseline_latency: baseline.latency,
            candidate_latency: self.latency,
            p50_delta_ms: millis_delta(self.latency.p50, baseline.latency.p50),
            p95_delta_ms: millis_delta(self.latency.p95, baseline.latency.p95),
        }
    }
}

impl Comparison {
    /// The comparison as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The comparison as Markdown: a table per evaluator with the deltas, the
    /// regressions and fixes, the cases only one report has, and the token and
    /// latency deltas. A delta or value that does not apply is `-`.
    pub fn to_markdown(&self) -> String {
        let mut md = format!(
            "# Comparison: {} vs {}\n\n{} regressions, {} fixes.\n\n",
            cell(&self.candidate_experiment),
            cell(&self.baseline_experiment),
            self.regressions.len(),
            self.fixes.len()
        );
        md.push_str(
            "| Evaluator | Baseline pass rate | Candidate pass rate | Pass rate delta | Baseline mean | Candidate mean | Mean delta |\n|---|---|---|---|---|---|---|\n",
        );
        for delta in &self.evaluators {
            let rate = |summary: &Option<EvaluatorSummary>| {
                summary
                    .as_ref()
                    .filter(|s| s.count > 0)
                    .map_or("-".to_owned(), |s| format!("{:.0}%", s.pass_rate * 100.0))
            };
            let mean = |summary: &Option<EvaluatorSummary>| {
                summary
                    .as_ref()
                    .filter(|s| s.count > 0)
                    .map_or("-".to_owned(), |s| format!("{:.2}", s.mean))
            };
            let pass_delta = delta
                .pass_rate_delta
                .map_or("-".to_owned(), |d| format!("{:+.1} pts", d * 100.0));
            let mean_delta = delta
                .mean_delta
                .map_or("-".to_owned(), |d| format!("{d:+.2}"));
            let _ = writeln!(
                md,
                "| {} | {} | {} | {pass_delta} | {} | {} | {mean_delta} |",
                cell(&delta.evaluator),
                rate(&delta.baseline),
                rate(&delta.candidate),
                mean(&delta.baseline),
                mean(&delta.candidate),
            );
        }
        md.push_str("\n## Regressions\n\n");
        push_changes(&mut md, &self.regressions, "No regressions.");
        md.push_str("\n## Fixes\n\n");
        push_changes(&mut md, &self.fixes, "No fixes.");
        md.push_str("\n## Cases in one report only\n\n");
        md.push_str("Only in the baseline: ");
        push_ids(&mut md, &self.only_in_baseline);
        md.push_str("\nOnly in the candidate: ");
        push_ids(&mut md, &self.only_in_candidate);
        let _ = writeln!(
            md,
            "\n\nTokens: {} baseline, {} candidate, {:+} delta. Latency: p50 {:+.1} ms, p95 {:+.1} ms (candidate minus baseline).",
            self.baseline_tokens.total,
            self.candidate_tokens.total,
            self.tokens_delta,
            self.p50_delta_ms,
            self.p95_delta_ms
        );
        md
    }
}

/// Each case's trials in `report`, by case id. Trials sharing an id are pooled.
fn cases(report: &Report) -> BTreeMap<&str, Vec<&TrialResult>> {
    let mut map: BTreeMap<&str, Vec<&TrialResult>> = BTreeMap::new();
    for result in &report.trials {
        map.entry(result.trial.case_id.as_str())
            .or_default()
            .push(result);
    }
    map
}

/// The evaluators that failed one of `trials`, in first-seen order, once each.
fn failing_evaluators(trials: &[&TrialResult]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for score in trials.iter().flat_map(|trial| trial.failures()) {
        if !names.contains(&score.evaluator) {
            names.push(score.evaluator.clone());
        }
    }
    names
}

fn evaluator_delta(
    name: &str,
    baseline: Option<&EvaluatorSummary>,
    candidate: Option<&EvaluatorSummary>,
) -> EvaluatorDelta {
    let both = baseline
        .zip(candidate)
        .filter(|(base, cand)| base.count > 0 && cand.count > 0);
    EvaluatorDelta {
        evaluator: name.to_owned(),
        baseline: baseline.cloned(),
        candidate: candidate.cloned(),
        pass_rate_delta: both.map(|(base, cand)| cand.pass_rate - base.pass_rate),
        mean_delta: both.map(|(base, cand)| cand.mean - base.mean),
    }
}

/// `candidate - baseline`, saturating at the `i64` bounds.
fn signed_delta(candidate: u64, baseline: u64) -> i64 {
    let delta = i128::from(candidate) - i128::from(baseline);
    i64::try_from(delta).unwrap_or(if delta < 0 { i64::MIN } else { i64::MAX })
}

/// `candidate - baseline` in milliseconds.
fn millis_delta(candidate: Duration, baseline: Duration) -> f64 {
    candidate.as_secs_f64() * 1000.0 - baseline.as_secs_f64() * 1000.0
}

/// `text` as a Markdown code span on one line. The fence is one backtick
/// longer than the longest backtick run in `text`, so the span cannot end early.
fn code_span(text: &str) -> String {
    let text = one_line(text);
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

fn push_changes(md: &mut String, changes: &[CaseChange], none: &str) {
    if changes.is_empty() {
        md.push_str(none);
        md.push('\n');
    }
    for change in changes {
        let evaluators: Vec<String> = change
            .evaluators
            .iter()
            .map(|name| one_line(name))
            .collect();
        let _ = writeln!(
            md,
            "- {}: {}",
            code_span(&change.case_id),
            evaluators.join(", ")
        );
    }
}

fn push_ids(md: &mut String, ids: &[String]) {
    if ids.is_empty() {
        md.push_str("none");
    } else {
        let spans: Vec<String> = ids.iter().map(|id| code_span(id)).collect();
        md.push_str(&spans.join(", "));
    }
}
