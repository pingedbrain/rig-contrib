//! The trial timeout: a target run or an evaluator call that never answers
//! fails its trial, and the other trials run on.

use std::time::Duration;

use rig_agent::AgentBuilder;
use rig_agent::test_utils::{MockAddTool, MockToolError};
use rig_agent::tool::{Tool, ToolContext};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_core::wasm_compat::WasmBoxedFuture;
use rig_evals::{
    Case, Dataset, EvalError, Evaluator, Experiment, Score, TargetError, Trial, Turn, evaluators,
    target_fn,
};
use serde_json::{Value, json};

/// Cases `ok-1`, `hang`, `ok-2`: the target never answers `hang`.
fn cases() -> Dataset {
    ["ok-1", "hang", "ok-2"]
        .into_iter()
        .map(|id| Case::new(id, "input"))
        .collect()
}

#[tokio::test]
async fn a_hung_target_run_times_out_and_the_other_trials_pass() {
    let report = Experiment::new("hung")
        .dataset(cases())
        .target(target_fn(|case: Case| async move {
            if case.id == "hang" {
                return std::future::pending::<Result<String, TargetError>>().await;
            }
            Ok::<_, TargetError>("ok".to_owned())
        }))
        .evaluator(evaluators::contains("ok"))
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_millis(200))
        .concurrency(3)
        .threshold("contains", 0.5)
        .threshold("no_error", 0.5)
        .run()
        .await
        .expect("the experiment runs");

    let hung = report
        .trials
        .iter()
        .find(|t| t.trial.case_id == "hang")
        .expect("a trial for hang");
    assert_eq!(
        hung.trial.error.as_deref(),
        Some("timed out after 0.2s"),
        "the trial says why"
    );
    assert!(hung.trial.latency >= Duration::from_millis(200));
    assert!(
        hung.scores.iter().all(|s| !s.passed),
        "a failed target fails every score"
    );

    for trial in report.trials.iter().filter(|t| t.trial.case_id != "hang") {
        assert_eq!(trial.trial.error, None);
        assert!(trial.scores.iter().all(|s| s.passed));
    }

    // The timed-out trial counts against the pass rate: 2 of 3 pass.
    assert_eq!(report.pass_rate("contains"), Some(2.0 / 3.0));
    assert_eq!(report.pass_rate("no_error"), Some(2.0 / 3.0));
    assert!(report.passed(), "2/3 reaches the 0.5 thresholds");
}

#[tokio::test]
async fn the_limit_is_per_trial_not_for_the_whole_run() {
    // One at a time, three trials of 100 ms each under a 250 ms limit: the
    // run takes longer than the limit, and no trial is over it.
    let report = Experiment::new("slow")
        .dataset(cases())
        .target(target_fn(|_: Case| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, TargetError>("ok".to_owned())
        }))
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_millis(250))
        .concurrency(1)
        .run()
        .await
        .expect("the experiment runs");

    assert_eq!(report.pass_rate("no_error"), Some(1.0));
}

#[tokio::test]
async fn without_a_timeout_nothing_changes() {
    let report = Experiment::new("untimed")
        .dataset(Dataset::new([Case::new("slow", "input")]))
        .target(target_fn(|_: Case| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, TargetError>("ok".to_owned())
        }))
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect("the experiment runs");

    let trial = &report.trials.first().expect("a trial").trial;
    assert_eq!(trial.error, None);
}

/// A judge that never returns a verdict.
struct Silent;

impl Evaluator for Silent {
    fn name(&self) -> &str {
        "silent_judge"
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Case,
        _: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_hung_evaluator_is_a_failed_score_labelled_error() {
    let report = Experiment::new("judged")
        .dataset(Dataset::new([Case::new("one", "input")]))
        .target(target_fn(|_: Case| async move {
            Ok::<_, TargetError>("answer".to_owned())
        }))
        .evaluator(Silent)
        .trial_timeout(Duration::from_millis(100))
        .run()
        .await
        .expect("the experiment runs");

    let score = report
        .trials
        .first()
        .and_then(|t| t.scores.first())
        .expect("a score");
    assert!(!score.passed);
    assert_eq!(score.label.as_deref(), Some("error"));
    assert_eq!(score.explanation.as_deref(), Some("timed out after 0.1s"));
    assert_eq!(
        report.trials.first().and_then(|t| t.trial.error.as_ref()),
        None
    );
}

/// Hangs when asked to run: a tool that never returns.
struct Hang;

impl Tool for Hang {
    const NAME: &'static str = "hang";
    type Error = MockToolError;
    type Args = Value;
    type Output = String;

    fn description(&self) -> String {
        "Never returns".to_owned()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn an_agent_turn_that_hangs_keeps_the_turns_before_it() {
    // Turn 1 answers; turn 2 calls a tool that never returns.
    let model = MockCompletionModel::from_turns([
        MockTurn::text("Lisbon.").with_usage(rig_core::completion::Usage::new().total_tokens(7)),
        MockTurn::tool_call("call-1", "hang", json!({})),
    ]);
    let agent = AgentBuilder::new(model)
        .tool(Hang)
        .default_max_turns(3)
        .build();

    let report = Experiment::new("agent-hang")
        .dataset(Dataset::new([Case::multi_turn(
            "chat",
            ["Where am I?", "Wait, and tell me more."],
        )]))
        .target(agent)
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_millis(200))
        .run()
        .await
        .expect("the experiment runs");

    let trial = &report.trials.first().expect("a trial").trial;
    assert_eq!(trial.error.as_deref(), Some("turn 2: timed out after 0.2s"));
    assert_eq!(
        trial.output, "Lisbon.",
        "the answer of the last completed turn"
    );
    assert_eq!(trial.transcript, [Turn::new("Where am I?", "Lisbon.")]);
    assert_eq!(trial.usage.total_tokens, Some(7));
    assert_eq!(report.pass_rate("no_error"), Some(0.0));
}

#[tokio::test]
async fn a_single_turn_agent_that_hangs_says_only_the_time() {
    let model = MockCompletionModel::from_turns([MockTurn::tool_call("call-1", "hang", json!({}))]);
    let agent = AgentBuilder::new(model)
        .tool(Hang)
        .default_max_turns(3)
        .build();

    let report = Experiment::new("agent-hang-single")
        .dataset(Dataset::new([Case::new("one", "Go")]))
        .target(agent)
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_millis(200))
        .run()
        .await
        .expect("the experiment runs");

    let trial = &report.trials.first().expect("a trial").trial;
    assert_eq!(trial.error.as_deref(), Some("timed out after 0.2s"));
}

#[tokio::test]
async fn the_agent_still_runs_normally_under_a_timeout() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "add", json!({"x": 2, "y": 3})),
        MockTurn::text("5"),
    ]);
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .default_max_turns(3)
        .build();

    let report = Experiment::new("agent-ok")
        .dataset(Dataset::new([Case::new("add", "2 + 3?")]))
        .target(agent)
        .evaluator(evaluators::contains("5"))
        .trial_timeout(Duration::from_secs(5))
        .run()
        .await
        .expect("the experiment runs");

    assert!(report.passed());
}

#[tokio::test]
async fn a_limit_too_large_for_the_clock_is_no_limit() {
    let report = Experiment::new("unbounded")
        .dataset(Dataset::new([Case::new("one", "input")]))
        .target(target_fn(|_: Case| async move {
            Ok::<_, TargetError>("ok".to_owned())
        }))
        .evaluator(evaluators::contains("ok"))
        .trial_timeout(Duration::MAX)
        .run()
        .await
        .expect("the experiment runs");

    let trial = &report.trials.first().expect("a trial").trial;
    assert_eq!(trial.error, None);
    assert!(report.passed());
}

#[tokio::test]
async fn a_nested_untimed_run_does_not_inherit_the_outer_deadline() {
    /// A tool that takes longer than the outer trial's limit.
    struct Slow;

    impl Tool for Slow {
        const NAME: &'static str = "slow";
        type Error = MockToolError;
        type Args = Value;
        type Output = String;

        fn description(&self) -> String {
            "Takes a while".to_owned()
        }

        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        async fn call(
            &self,
            _context: &mut ToolContext,
            _args: Self::Args,
        ) -> Result<Self::Output, Self::Error> {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok("done".to_owned())
        }
    }

    // The outer trial has 100 ms. Its target runs an inner experiment with no
    // timeout; the inner agent's tool takes 150 ms. The inner run must not be
    // cut at the outer deadline, so the outer trial is the one that times out.
    let outer = Experiment::new("outer")
        .dataset(Dataset::new([Case::new("one", "input")]))
        .target(target_fn(|_: Case| async move {
            let model = MockCompletionModel::from_turns([
                MockTurn::tool_call("call-1", "slow", json!({})),
                MockTurn::text("answer"),
            ]);
            let agent = AgentBuilder::new(model)
                .tool(Slow)
                .default_max_turns(3)
                .build();
            let inner = Experiment::new("inner")
                .dataset(Dataset::new([Case::new("inner-case", "go")]))
                .target(agent)
                .evaluator(evaluators::no_error())
                .run()
                .await
                .map_err(|error| -> TargetError { error.to_string().into() })?;
            let answer = inner.trials.first().and_then(|t| t.trial.error.clone());
            Ok::<_, TargetError>(answer.unwrap_or_else(|| "inner ok".to_owned()))
        }))
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_millis(100))
        .run()
        .await
        .expect("the experiment runs");

    let trial = &outer.trials.first().expect("a trial").trial;
    assert_eq!(trial.error.as_deref(), Some("timed out after 0.1s"));
}
