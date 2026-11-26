//! Running experiments: repetitions, pass rates, thresholds, concurrency, and
//! failures that do not stop the run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rig_core::wasm_compat::WasmBoxedFuture;
use rig_evals::{
    Case, Dataset, EvalError, Evaluator, Experiment, ExperimentError, JsonFile, Score, TargetError,
    Trial, evaluators, target_fn,
};

fn cases(n: usize) -> Dataset {
    (0..n)
        .map(|i| Case::new(format!("case-{i}"), format!("input {i}")))
        .collect()
}

/// Answers "good" except on its second call.
fn flaky() -> Experiment {
    let calls = Arc::new(AtomicUsize::new(0));
    Experiment::new("flaky")
        .dataset(cases(1))
        .target(target_fn(move |_: Case| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok::<_, TargetError>(if call == 1 { "bad" } else { "good" }) }
        }))
        .evaluator(evaluators::contains("good"))
        .repetitions(4)
}

#[tokio::test]
async fn repetitions_give_a_pass_rate_that_thresholds_gate() {
    let report = flaky()
        .threshold("contains", 0.8)
        .run()
        .await
        .expect("runs");

    assert_eq!(report.trials.len(), 4);
    let repetitions: Vec<usize> = report.trials.iter().map(|t| t.trial.repetition).collect();
    assert_eq!(repetitions, [1, 2, 3, 4]);
    assert_eq!(report.pass_rate("contains"), Some(0.75));
    let summary = report.summary.first().expect("one evaluator");
    assert_eq!((summary.count, summary.passed), (4, 3));
    assert_eq!(summary.mean, 0.75);
    assert!(!report.passed(), "0.75 is below 0.8");

    let report = flaky()
        .threshold("contains", 0.7)
        .run()
        .await
        .expect("runs");
    assert!(report.passed(), "0.75 reaches 0.7");
}

#[tokio::test]
async fn without_a_threshold_every_run_must_pass() {
    let report = flaky().run().await.expect("runs");
    let threshold = report.thresholds.first().expect("one per evaluator");
    assert_eq!(threshold.min_pass_rate, 1.0);
    assert!(!report.passed());
}

#[tokio::test]
async fn concurrency_bounds_the_runs_in_flight() {
    for limit in [1, 3] {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (running_in, peak_in) = (running.clone(), peak.clone());
        let report = Experiment::new("bounded")
            .dataset(cases(10))
            .target(target_fn(move |case: Case| {
                let (running, peak) = (running_in.clone(), peak_in.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, TargetError>(case.input)
                }
            }))
            .evaluator(evaluators::no_error())
            .concurrency(limit)
            .run()
            .await
            .expect("runs");

        assert_eq!(peak.load(Ordering::SeqCst), limit);
        let ids: Vec<&str> = report
            .trials
            .iter()
            .map(|t| t.trial.case_id.as_str())
            .collect();
        let expected: Vec<String> = (0..10).map(|i| format!("case-{i}")).collect();
        assert_eq!(ids, expected, "the report keeps the dataset's order");
        assert!(
            report
                .trials
                .iter()
                .all(|t| t.trial.latency >= Duration::from_millis(10))
        );
    }
}

struct Broken;

impl Evaluator for Broken {
    fn name(&self) -> &str {
        "broken"
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Case,
        _: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        Box::pin(async { Err(EvalError::Other("the grader is offline".into())) })
    }
}

#[tokio::test]
async fn an_evaluator_error_is_a_failed_score_and_the_rest_still_run() {
    let report = Experiment::new("errors")
        .dataset(cases(2))
        .target(target_fn(|case: Case| async move {
            Ok::<_, TargetError>(case.input)
        }))
        .evaluator(Broken)
        .evaluator(evaluators::equals())
        .evaluator(evaluators::contains("input"))
        .run()
        .await
        .expect("runs");

    for result in &report.trials {
        let [broken, equals, contains] = result.scores.as_slice() else {
            panic!("three scores: {:?}", result.scores);
        };
        assert!(!broken.passed);
        assert_eq!(broken.label.as_deref(), Some("error"));
        assert_eq!(broken.explanation.as_deref(), Some("the grader is offline"));
        assert!(!equals.passed, "no expected answer is an error");
        assert_eq!(equals.label.as_deref(), Some("error"));
        assert!(contains.passed);
    }
}

#[tokio::test]
async fn a_failing_target_makes_a_trial_with_its_error() {
    let report = Experiment::new("down")
        .dataset(cases(1))
        .target(target_fn(|_: Case| async {
            Err::<String, TargetError>("connection refused".into())
        }))
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect("runs");
    let result = report.trials.first().expect("a trial");
    assert_eq!(result.trial.error.as_deref(), Some("connection refused"));
    assert!(!result.passed());
    assert!(!report.passed());
}

#[tokio::test]
async fn a_misconfigured_experiment_does_not_run() {
    let echo = || target_fn(|case: Case| async move { Ok::<_, TargetError>(case.input) });
    let base = || Experiment::new("x").dataset(cases(1)).target(echo());

    assert!(matches!(
        Experiment::new("x")
            .dataset(cases(1))
            .evaluator(evaluators::no_error())
            .run()
            .await,
        Err(ExperimentError::NoTarget)
    ));
    assert!(matches!(
        base().run().await,
        Err(ExperimentError::NoEvaluators)
    ));
    assert!(matches!(
        base()
            .evaluator(evaluators::no_error())
            .threshold("contains", 0.5)
            .run()
            .await,
        Err(ExperimentError::UnknownThreshold(name)) if name == "contains"
    ));
    assert!(matches!(
        base()
            .evaluator(evaluators::contains("a"))
            .evaluator(evaluators::contains("b"))
            .run()
            .await,
        Err(ExperimentError::DuplicateEvaluator(name)) if name == "contains"
    ));
    assert!(matches!(
        base()
            .evaluator(evaluators::no_error())
            .threshold("no_error", 1.5)
            .run()
            .await,
        Err(ExperimentError::InvalidThreshold { .. })
    ));
    assert!(matches!(
        Experiment::new("x")
            .dataset(vec![Case::new("a", "1"), Case::new("a", "2")])
            .target(echo())
            .evaluator(evaluators::no_error())
            .run()
            .await,
        Err(ExperimentError::DuplicateCase(id)) if id == "a"
    ));

    // Renamed, two of a kind are fine.
    let report = base()
        .evaluator(evaluators::contains("input"))
        .evaluator(evaluators::contains("nope").named("contains_nope"))
        .threshold("contains_nope", 0.0)
        .run()
        .await
        .expect("runs");
    assert_eq!(report.pass_rate("contains_nope"), Some(0.0));
    assert!(report.passed());
}

#[tokio::test]
async fn a_failed_sink_still_returns_the_report() {
    let file = tempfile::NamedTempFile::new().expect("tempfile");
    let error = Experiment::new("sink")
        .dataset(cases(1))
        .target(target_fn(|case: Case| async move {
            Ok::<_, TargetError>(case.input)
        }))
        .evaluator(evaluators::no_error())
        .sink(JsonFile::new(file.path().join("report.json")))
        .run()
        .await
        .expect_err("a file is not a directory");
    let ExperimentError::Sink { report, .. } = error else {
        panic!("a sink error: {error}");
    };
    assert_eq!(report.trials.len(), 1);
}

#[tokio::test]
async fn an_experiment_runs_on_a_spawned_task() {
    let report = tokio::spawn(async { flaky().threshold("contains", 0.5).run().await })
        .await
        .expect("joins")
        .expect("runs");
    assert!(report.passed());
}
