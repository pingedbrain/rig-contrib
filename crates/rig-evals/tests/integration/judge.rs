//! A scripted judge model grading answers.

use rig_agent::AgentBuilder;
use rig_core::completion::FinishReason;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{
    Case, Evaluator, Experiment, LlmJudge, TargetError, TargetOutput, Trial, target_fn,
};

use crate::tokens;

fn judge(verdict: &str) -> (LlmJudge, MockCompletionModel) {
    let model = MockCompletionModel::from_turns([MockTurn::text(verdict)]);
    let agent = AgentBuilder::new(model.clone()).build();
    (
        LlmJudge::new(agent, "Is the answer polite and correct?").threshold(0.7),
        model,
    )
}

fn case() -> Case {
    Case::new("capital", "What is the capital of France?")
        .expected("Paris")
        .assertion("names Paris")
}

#[tokio::test]
async fn a_verdict_above_the_threshold_passes_with_the_reasoning() {
    let (judge, model) = judge(r#"{"score": 0.9, "reasoning": "Correct and polite."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "It is Paris."))
        .await
        .expect("a verdict");

    assert!(score.passed);
    assert_eq!(score.value, 0.9);
    assert_eq!(score.explanation.as_deref(), Some("Correct and polite."));

    let request = model.requests().into_iter().next().expect("one request");
    let sent = serde_json::to_string(&request.chat_history).expect("json");
    for part in [
        "Is the answer polite and correct?",
        "What is the capital of France?",
        "Paris",
        "names Paris",
        "It is Paris.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}");
    }
}

#[tokio::test]
async fn a_verdict_below_the_threshold_fails() {
    let (judge, _) = judge(r#"{"score": 0.4, "reasoning": "Rude."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris, obviously."))
        .await
        .expect("a verdict");
    assert!(!score.passed);
    assert_eq!(score.value, 0.4);
}

#[tokio::test]
async fn an_out_of_range_score_is_clamped() {
    let (judge, _) = judge(r#"{"score": 7, "reasoning": "Great."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a verdict");
    assert_eq!(score.value, 1.0);
    assert!(score.passed);
}

#[tokio::test]
async fn no_verdict_is_a_failed_score_labelled_error() {
    let (judge, _) = judge("I think it is fine");
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a scored failure, not an evaluator error");
    assert!(!score.passed);
    assert_eq!(score.label.as_deref(), Some("error"));
    let why = score.explanation.expect("why");
    assert!(why.starts_with("the judge returned no verdict: "), "{why}");
    assert!(!why.contains("truncated"), "{why}");
}

/// A judge whose one scripted verdict reports 30 input and 5 output tokens.
fn judge_with_usage() -> LlmJudge {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.9, "reasoning": "Fine."}"#,
    )
    .with_usage(tokens(30, 5))]);
    LlmJudge::new(AgentBuilder::new(model).build(), "Is it correct?")
}

#[tokio::test]
async fn the_judges_usage_is_on_its_score_and_in_the_report_totals() {
    let score = judge_with_usage()
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a verdict");
    assert_eq!(score.usage, Some(tokens(30, 5)));

    let report = Experiment::new("judged")
        .dataset(vec![case()])
        .target(target_fn(|_case: Case| async {
            Ok::<_, TargetError>(TargetOutput::new("Paris").with_usage(tokens(10, 2)))
        }))
        .evaluator(judge_with_usage())
        .run()
        .await
        .expect("runs");
    assert_eq!(
        (
            report.judge_tokens.input,
            report.judge_tokens.output,
            report.judge_tokens.total
        ),
        (30, 5, 35)
    );
    assert_eq!(
        (
            report.tokens.input,
            report.tokens.output,
            report.tokens.total
        ),
        (40, 7, 47),
        "the target's tokens and the judge's"
    );
    assert!(
        report
            .to_markdown()
            .contains("Of which LLM judges: 30 input, 5 output, 35 total.")
    );
}

/// A judge whose reply stops at its token cap: the scripted reply is cut
/// mid-object, marked as stopped for length, and reports the tokens it spent.
#[tokio::test]
async fn a_reply_cut_at_max_tokens_says_so_and_keeps_its_usage() {
    let model = MockCompletionModel::from_turns([MockTurn::text(r#"{"score": 0.9, "reas"#)
        .with_finish_reason(FinishReason::Length)
        .with_usage(tokens(40, 64))]);
    let agent = AgentBuilder::new(model).max_tokens(64).build();
    let judge = LlmJudge::new(agent, "Is it correct?");
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a scored failure");
    assert!(!score.passed);
    assert_eq!(score.label.as_deref(), Some("error"));
    assert_eq!(
        score.explanation.as_deref(),
        Some("the judge returned no verdict: the reply was truncated at max_tokens (64)")
    );
    assert_eq!(score.usage, Some(tokens(40, 64)));
}
