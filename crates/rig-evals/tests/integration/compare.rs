//! Comparing a candidate report with a baseline, on two hand-built reports
//! (loaded from JSON, as a saved baseline would be).

use rig_evals::{Comparison, Report};
use serde_json::{Value, json};

/// One trial of `case` with one score per `(evaluator, passed)`.
fn trial(case: &str, scores: &[(&str, bool)]) -> Value {
    let scores: Vec<Value> = scores
        .iter()
        .map(|(evaluator, passed)| {
            json!({
                "evaluator": evaluator,
                "value": if *passed { 1.0 } else { 0.0 },
                "passed": passed,
            })
        })
        .collect();
    json!({
        "trial": {
            "case_id": case,
            "repetition": 1,
            "output": "",
            "tool_calls": [],
            "usage": {},
            "latency_ms": 100.0,
        },
        "scores": scores,
    })
}

fn summary(evaluator: &str, count: usize, passed: usize, mean: f64) -> Value {
    json!({
        "evaluator": evaluator,
        "count": count,
        "passed": passed,
        "pass_rate": if count == 0 { 0.0 } else { passed as f64 / count as f64 },
        "mean": mean,
    })
}

fn report(
    experiment: &str,
    trials: Vec<Value>,
    summary: Vec<Value>,
    total: u64,
    p50: f64,
    p95: f64,
) -> Report {
    let thresholds: Vec<Value> = summary
        .iter()
        .map(|summary| {
            json!({
                "evaluator": summary["evaluator"],
                "min_pass_rate": 1.0,
                "pass_rate": summary["pass_rate"],
                "passed": summary["pass_rate"] == json!(1.0),
            })
        })
        .collect();
    let value = json!({
        "experiment": experiment,
        "repetitions": 1,
        "trials": trials,
        "summary": summary,
        "tokens": { "input": total - total / 6, "output": total / 6, "total": total },
        "latency": { "p50_ms": p50, "p95_ms": p95 },
        "thresholds": thresholds,
    });
    serde_json::from_value(value).expect("a valid report")
}

/// Yesterday's run: `city` fails `contains`.
fn baseline() -> Report {
    report(
        "support-v1",
        vec![
            trial("refund", &[("contains", true), ("no_error", true)]),
            trial("hours", &[("contains", true), ("no_error", true)]),
            trial("city", &[("contains", false), ("no_error", true)]),
        ],
        vec![
            summary("contains", 3, 2, 2.0 / 3.0),
            summary("no_error", 3, 3, 1.0),
        ],
        360,
        100.0,
        150.0,
    )
}

/// Today's run: `refund` now fails `contains` (a regression), `city` passes
/// (a fix), and `new` is only in this run.
fn candidate() -> Report {
    report(
        "support-v2",
        vec![
            trial("refund", &[("contains", false), ("no_error", true)]),
            trial("hours", &[("contains", true), ("no_error", true)]),
            trial("city", &[("contains", true), ("no_error", true)]),
            trial("new", &[("contains", true), ("no_error", true)]),
        ],
        vec![
            summary("contains", 4, 3, 0.75),
            summary("no_error", 4, 4, 1.0),
        ],
        480,
        120.0,
        130.0,
    )
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{actual} is not {expected}"
    );
}

#[test]
fn flips_and_case_membership_are_matched_by_id() {
    let comparison = candidate().compare(&baseline());

    assert_eq!(comparison.baseline_experiment, "support-v1");
    assert_eq!(comparison.candidate_experiment, "support-v2");
    let [regression] = comparison.regressions.as_slice() else {
        panic!("one regression, got {:?}", comparison.regressions);
    };
    assert_eq!(regression.case_id, "refund");
    assert_eq!(regression.evaluators, ["contains"]);
    let [fix] = comparison.fixes.as_slice() else {
        panic!("one fix, got {:?}", comparison.fixes);
    };
    assert_eq!(fix.case_id, "city");
    assert_eq!(fix.evaluators, ["contains"]);
    assert!(comparison.only_in_baseline.is_empty());
    assert_eq!(comparison.only_in_candidate, ["new"]);
}

#[test]
fn evaluator_and_cost_deltas() {
    let comparison = candidate().compare(&baseline());

    let [contains, no_error] = comparison.evaluators.as_slice() else {
        panic!("two evaluators, got {:?}", comparison.evaluators);
    };
    assert_eq!(contains.evaluator, "contains");
    approx(contains.pass_rate_delta.expect("in both"), 0.75 - 2.0 / 3.0);
    approx(contains.mean_delta.expect("in both"), 0.75 - 2.0 / 3.0);
    approx(no_error.pass_rate_delta.expect("in both"), 0.0);
    approx(no_error.mean_delta.expect("in both"), 0.0);

    assert_eq!(comparison.baseline_tokens.total, 360);
    assert_eq!(comparison.candidate_tokens.total, 480);
    assert_eq!(comparison.tokens_delta, 120);
    approx(comparison.p50_delta_ms, 20.0);
    approx(comparison.p95_delta_ms, -20.0);
}

#[test]
fn a_run_compared_with_itself_has_no_changes() {
    let same = baseline().compare(&baseline());
    assert!(same.regressions.is_empty());
    assert!(same.fixes.is_empty());
    assert!(same.only_in_baseline.is_empty());
    assert!(same.only_in_candidate.is_empty());
    assert!(
        same.evaluators
            .iter()
            .all(|delta| delta.pass_rate_delta == Some(0.0))
    );
    assert_eq!(same.tokens_delta, 0);
}

#[test]
fn markdown_names_the_regression_fix_and_new_case() {
    let md = candidate().compare(&baseline()).to_markdown();

    assert!(
        md.starts_with("# Comparison: support-v2 vs support-v1\n"),
        "{md}"
    );
    assert!(md.contains("1 regressions, 1 fixes."), "{md}");
    assert!(
        md.contains("| contains | 67% | 75% | +8.3 pts | 0.67 | 0.75 | +0.08 |"),
        "{md}"
    );
    assert!(md.contains("- `refund`: contains"), "{md}");
    assert!(md.contains("- `city`: contains"), "{md}");
    assert!(md.contains("Only in the candidate: `new`"), "{md}");
    assert!(md.contains("Only in the baseline: none"), "{md}");
}

#[test]
fn comparison_and_saved_reports_round_trip_through_json() {
    let baseline = baseline();
    let comparison = candidate().compare(&baseline);

    let json = comparison.to_json().expect("serializes");
    let back: Comparison = serde_json::from_str(&json).expect("parses back");
    assert_eq!(back, comparison);

    // A baseline saved to a file loads again as the same report.
    let saved = baseline.to_json().expect("serializes");
    let loaded: Report = serde_json::from_str(&saved).expect("loads");
    assert_eq!(loaded, baseline);
    assert_eq!(loaded.compare(&baseline).regressions.len(), 0);
}

#[test]
fn an_evaluator_with_no_scored_trials_has_no_delta() {
    let base = report("v1", vec![], vec![summary("judge", 0, 0, 0.0)], 0, 0.0, 0.0);
    let cand = report(
        "v2",
        vec![trial("a", &[("judge", true)])],
        vec![summary("judge", 1, 1, 1.0)],
        0,
        0.0,
        0.0,
    );
    let comparison = cand.compare(&base);

    let [judge] = comparison.evaluators.as_slice() else {
        panic!("one evaluator, got {:?}", comparison.evaluators);
    };
    assert_eq!(judge.pass_rate_delta, None);
    assert_eq!(judge.mean_delta, None);
    let md = comparison.to_markdown();
    assert!(
        md.contains("| judge | - | 100% | - | - | 1.00 | - |"),
        "{md}"
    );
}

#[test]
fn an_evaluator_in_one_report_only_has_no_delta() {
    let base = report(
        "v1",
        vec![trial("a", &[("contains", true), ("length", true)])],
        vec![summary("contains", 1, 1, 1.0), summary("length", 1, 1, 1.0)],
        0,
        0.0,
        0.0,
    );
    let cand = report(
        "v2",
        vec![trial("a", &[("contains", true), ("judge", false)])],
        vec![summary("contains", 1, 1, 1.0), summary("judge", 1, 0, 0.0)],
        0,
        0.0,
        0.0,
    );
    let comparison = cand.compare(&base);

    let [contains, length, judge] = comparison.evaluators.as_slice() else {
        panic!("three evaluators, got {:?}", comparison.evaluators);
    };
    assert_eq!(contains.pass_rate_delta, Some(0.0));
    // Only the baseline has `length`.
    assert!(length.baseline.is_some());
    assert!(length.candidate.is_none());
    assert_eq!(length.pass_rate_delta, None);
    assert_eq!(length.mean_delta, None);
    // Only the candidate has `judge`.
    assert!(judge.baseline.is_none());
    assert!(judge.candidate.is_some());
    assert_eq!(judge.pass_rate_delta, None);
    assert_eq!(judge.mean_delta, None);

    let md = comparison.to_markdown();
    assert!(
        md.contains("| length | 100% | - | - | 1.00 | - | - |"),
        "{md}"
    );
    assert!(md.contains("| judge | - | 0% | - | - | 0.00 | - |"), "{md}");
}

#[test]
fn trials_sharing_a_case_id_are_pooled_into_one_case() {
    // Baseline: `dup` has a passing and a failing trial, so the case fails.
    let base = report(
        "v1",
        vec![
            trial("dup", &[("contains", true)]),
            trial("dup", &[("contains", false)]),
        ],
        vec![summary("contains", 2, 1, 0.5)],
        0,
        0.0,
        0.0,
    );
    // Candidate: both trials pass, so the case passes.
    let cand = report(
        "v2",
        vec![
            trial("dup", &[("contains", true)]),
            trial("dup", &[("contains", true)]),
        ],
        vec![summary("contains", 2, 2, 1.0)],
        0,
        0.0,
        0.0,
    );

    let fixed = cand.compare(&base);
    assert!(fixed.regressions.is_empty());
    let [fix] = fixed.fixes.as_slice() else {
        panic!("one fix, got {:?}", fixed.fixes);
    };
    assert_eq!(fix.case_id, "dup");
    assert_eq!(fix.evaluators, ["contains"]);
    assert!(fixed.only_in_baseline.is_empty());
    assert!(fixed.only_in_candidate.is_empty());

    // The reverse direction is one regression, not one per trial.
    let regressed = base.compare(&cand);
    assert!(regressed.fixes.is_empty());
    let [regression] = regressed.regressions.as_slice() else {
        panic!("one regression, got {:?}", regressed.regressions);
    };
    assert_eq!(regression.case_id, "dup");
    assert_eq!(regression.evaluators, ["contains"]);
}

#[test]
fn two_empty_reports_compare_with_no_changes_and_finite_deltas() {
    let empty = report("empty", vec![], vec![], 0, 0.0, 0.0);
    let comparison = empty.compare(&empty);

    assert!(comparison.evaluators.is_empty());
    assert!(comparison.regressions.is_empty());
    assert!(comparison.fixes.is_empty());
    assert!(comparison.only_in_baseline.is_empty());
    assert!(comparison.only_in_candidate.is_empty());
    assert_eq!(comparison.tokens_delta, 0);
    assert!(comparison.p50_delta_ms.is_finite());
    assert!(comparison.p95_delta_ms.is_finite());
    assert!(comparison.to_markdown().contains("0 regressions, 0 fixes."));
}

#[test]
fn case_ids_are_sanitized_in_markdown_lists() {
    let id = "a|b\nc`d";
    let base = report(
        "v1",
        vec![trial(id, &[("contains", true)])],
        vec![summary("contains", 1, 1, 1.0)],
        0,
        0.0,
        0.0,
    );
    let cand = report(
        "v2",
        vec![
            trial(id, &[("contains", false)]),
            trial("`tick", &[("contains", true)]),
        ],
        vec![summary("contains", 2, 1, 0.5)],
        0,
        0.0,
        0.0,
    );
    let md = cand.compare(&base).to_markdown();

    // One line, the backtick kept inside a longer fence, the pipe as is.
    assert!(md.contains("- ``a|b c`d``: contains"), "{md}");
    assert!(md.lines().any(|line| line.starts_with("- ``a|b c")), "{md}");
    assert!(md.contains("Only in the candidate: `` `tick ``"), "{md}");
}
