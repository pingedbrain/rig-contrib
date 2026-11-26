//! A scripted judge model grading answers.

use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{Case, EvalError, Evaluator, LlmJudge, Trial};

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
async fn no_verdict_is_a_judge_error() {
    let (judge, _) = judge("I think it is fine");
    let error = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect_err("not a verdict");
    assert!(matches!(error, EvalError::Judge(_)), "{error}");
}
