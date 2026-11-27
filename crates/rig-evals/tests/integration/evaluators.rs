//! Each built-in evaluator, passing and failing.

use std::time::Duration;

use rig_evals::evaluators::{self, TrajectoryMode};
use rig_evals::{Case, EvalError, Evaluator, ExpectedCall, Score, ToolCall, Trial};
use serde_json::json;

use crate::tokens;

async fn score(evaluator: &impl Evaluator, case: &Case, trial: &Trial) -> Score {
    evaluator.evaluate(case, trial).await.expect("scores")
}

fn case() -> Case {
    Case::new("c", "input")
}

fn answer(output: &str) -> Trial {
    Trial::new("c", 1, output)
}

fn failed() -> Trial {
    let mut trial = answer("");
    trial.error = Some("model down".into());
    trial
}

#[tokio::test]
async fn contains_checks_every_needle() {
    let pass = score(
        &evaluators::contains("refund"),
        &case(),
        &answer("A refund is due"),
    )
    .await;
    assert!(pass.passed);
    assert_eq!(pass.value, 1.0);

    let half = score(
        &evaluators::contains_all(["refund", "30 days"]),
        &case(),
        &answer("A refund is due"),
    )
    .await;
    assert!(!half.passed);
    assert_eq!(half.value, 0.5);
    assert!(half.explanation.expect("why").contains("30 days"));

    assert!(
        !score(&evaluators::contains("REFUND"), &case(), &answer("refund"))
            .await
            .passed
    );
    assert!(
        score(
            &evaluators::contains("REFUND").ignore_case(),
            &case(),
            &answer("refund")
        )
        .await
        .passed
    );
}

#[tokio::test]
async fn regex_matches_the_answer() {
    let order = evaluators::regex(r"order #\d+").expect("valid pattern");
    assert!(
        score(&order, &case(), &answer("See order #42"))
            .await
            .passed
    );
    assert!(!score(&order, &case(), &answer("See order #")).await.passed);
    assert!(evaluators::regex("(").is_err());
}

#[tokio::test]
async fn equals_compares_trimmed_with_expected() {
    let case = case().expected("Paris");
    assert!(
        score(&evaluators::equals(), &case, &answer("  Paris\n"))
            .await
            .passed
    );
    let wrong = score(&evaluators::equals(), &case, &answer("paris")).await;
    assert!(!wrong.passed);
    assert_eq!(wrong.value, 0.0);
    assert!(
        score(&evaluators::equals().ignore_case(), &case, &answer("paris"))
            .await
            .passed
    );

    let missing = evaluators::equals()
        .evaluate(&self::case(), &answer("Paris"))
        .await;
    assert!(matches!(
        missing,
        Err(EvalError::MissingReference {
            field: "expected",
            ..
        })
    ));
}

#[tokio::test]
async fn json_parses_and_checks_required_keys() {
    let json = evaluators::json().require("city").require("temp");
    assert!(
        score(&json, &case(), &answer(r#" {"city": "Oslo", "temp": 3} "#))
            .await
            .passed
    );
    assert!(
        score(&evaluators::json(), &case(), &answer("[1, 2]"))
            .await
            .passed
    );

    let missing = score(&json, &case(), &answer(r#"{"city": "Oslo"}"#)).await;
    assert!(!missing.passed);
    assert!(missing.explanation.expect("why").contains("temp"));
    assert!(!score(&json, &case(), &answer("[1]")).await.passed);
    let prose = score(&evaluators::json(), &case(), &answer("Oslo is cold")).await;
    assert!(prose.explanation.expect("why").starts_with("not JSON"));
}

fn called(calls: &[(&str, serde_json::Value)]) -> Trial {
    let mut trial = answer("done");
    trial.tool_calls = calls
        .iter()
        .map(|(name, args)| ToolCall::new(*name, args.clone()))
        .collect();
    trial
}

fn expecting(calls: impl IntoIterator<Item = ExpectedCall>) -> Case {
    case().trajectory(calls)
}

#[tokio::test]
async fn trajectory_exact_wants_the_same_calls_only() {
    let exact = evaluators::trajectory(TrajectoryMode::Exact);
    let case = expecting([ExpectedCall::new("search"), ExpectedCall::new("book")]);
    let both = called(&[("search", json!({"q": "x"})), ("book", json!({}))]);
    assert!(score(&exact, &case, &both).await.passed);

    let extra = called(&[
        ("search", json!({})),
        ("book", json!({})),
        ("email", json!({})),
    ]);
    let scored = score(&exact, &case, &extra).await;
    assert!(!scored.passed);
    assert!((scored.value - 2.0 / 3.0).abs() < 1e-9);
    let swapped = called(&[("book", json!({})), ("search", json!({}))]);
    assert!(!score(&exact, &case, &swapped).await.passed);
}

#[tokio::test]
async fn trajectory_in_order_allows_calls_between() {
    let in_order = evaluators::trajectory(TrajectoryMode::InOrder);
    let case = expecting([ExpectedCall::new("search"), ExpectedCall::new("book")]);
    let between = called(&[
        ("search", json!({})),
        ("weather", json!({})),
        ("book", json!({})),
    ]);
    assert!(score(&in_order, &case, &between).await.passed);

    let swapped = called(&[("book", json!({})), ("search", json!({}))]);
    let scored = score(&in_order, &case, &swapped).await;
    assert!(!scored.passed);
    assert_eq!(scored.value, 0.5);
}

#[tokio::test]
async fn trajectory_any_order_matches_each_call_once() {
    let any = evaluators::trajectory(TrajectoryMode::AnyOrder);
    let case = expecting([ExpectedCall::new("book"), ExpectedCall::new("search")]);
    let swapped = called(&[
        ("search", json!({})),
        ("weather", json!({})),
        ("book", json!({})),
    ]);
    assert!(score(&any, &case, &swapped).await.passed);
    assert!(
        !score(&any, &case, &called(&[("search", json!({}))]))
            .await
            .passed
    );

    // A call with any arguments must not take the one a specific call needs.
    let case = expecting([
        ExpectedCall::new("add"),
        ExpectedCall::new("add").args(json!({"x": 1})),
    ]);
    let calls = called(&[("add", json!({"x": 1, "y": 2})), ("add", json!({"x": 5}))]);
    assert!(score(&any, &case, &calls).await.passed);
}

#[tokio::test]
async fn trajectory_compares_given_args_as_a_subset() {
    let exact = evaluators::trajectory(TrajectoryMode::Exact);
    let case = expecting([ExpectedCall::new("add").args(json!({"x": 1}))]);
    assert!(
        score(&exact, &case, &called(&[("add", json!({"x": 1, "y": 2}))]))
            .await
            .passed
    );
    assert!(
        !score(&exact, &case, &called(&[("add", json!({"x": 2}))]))
            .await
            .passed
    );

    let missing = exact.evaluate(&self::case(), &called(&[])).await;
    assert!(matches!(
        missing,
        Err(EvalError::MissingReference {
            field: "trajectory",
            ..
        })
    ));
}

#[tokio::test]
async fn budget_bounds_tokens_and_latency() {
    let budget = evaluators::budget()
        .max_output_tokens(10)
        .max_total_tokens(100)
        .max_latency(Duration::from_secs(1));
    let mut within = answer("ok");
    within.usage = tokens(50, 10);
    within.latency = Duration::from_millis(200);
    assert!(score(&budget, &case(), &within).await.passed);

    let mut over = within.clone();
    over.usage = tokens(95, 11);
    over.latency = Duration::from_secs(2);
    let scored = score(&budget, &case(), &over).await;
    assert!(!scored.passed);
    let why = scored.explanation.expect("why");
    assert!(why.contains("output tokens: 11 > 10"), "{why}");
    assert!(why.contains("total tokens: 106 > 100"), "{why}");
    assert!(why.contains("latency"), "{why}");

    let unreported = score(
        &evaluators::budget().max_output_tokens(10),
        &case(),
        &answer("ok"),
    )
    .await;
    assert!(!unreported.passed);
}

#[tokio::test]
async fn no_error_and_every_built_in_fail_a_failed_target() {
    assert!(
        score(&evaluators::no_error(), &case(), &answer("ok"))
            .await
            .passed
    );
    let scored = score(&evaluators::no_error(), &case(), &failed()).await;
    assert!(!scored.passed);
    assert!(scored.explanation.expect("why").contains("model down"));

    let case = case().expected("").trajectory([]);
    assert!(
        !score(&evaluators::contains(""), &case, &failed())
            .await
            .passed
    );
    assert!(!score(&evaluators::equals(), &case, &failed()).await.passed);
    assert!(!score(&evaluators::budget(), &case, &failed()).await.passed);
    assert!(
        !score(
            &evaluators::trajectory(TrajectoryMode::Exact),
            &case,
            &failed()
        )
        .await
        .passed
    );
}

#[tokio::test]
async fn a_missing_reference_is_not_applicable_when_asked() {
    let equals = evaluators::equals().not_applicable_without_reference();
    let skipped = score(&equals, &case(), &answer("Paris")).await;
    assert!(skipped.not_applicable);
    assert!(!skipped.passed);
    assert_eq!(skipped.label.as_deref(), Some("n/a"));

    let exact = evaluators::trajectory(TrajectoryMode::Exact).not_applicable_without_reference();
    let skipped = score(&exact, &case(), &called(&[])).await;
    assert!(skipped.not_applicable);

    // With the reference present, the evaluator scores as usual.
    let scored = score(&equals, &case().expected("Paris"), &answer("Paris")).await;
    assert!(scored.passed && !scored.not_applicable);
}
