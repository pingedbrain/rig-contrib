//! A live run against an AgentCore runtime, skipped unless
//! `RIG_EVALS_AGENTCORE_RUNTIME_ARN` names one (with AWS credentials and
//! region in the environment). The runtime must serve a Rig agent with a
//! `calculate` tool, deployed with rig-agentcore and rig-otel recording
//! content.

use rig_core::wasm_compat::WasmBoxedFuture;
use rig_evals::{
    AgentCore, Case, Dataset, EvalError, Evaluator, Experiment, Score, Trial, evaluators,
};

/// The built-in `contains`, on each case's expected answer.
struct ContainsExpected;

impl Evaluator for ContainsExpected {
    fn name(&self) -> &str {
        "contains"
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        let contains = evaluators::contains(case.expected.clone().unwrap_or_default());
        Box::pin(async move { contains.evaluate(case, trial).await })
    }
}

#[tokio::test]
async fn agentcore_evaluators_score_a_live_runtime() {
    let Ok(arn) = std::env::var("RIG_EVALS_AGENTCORE_RUNTIME_ARN") else {
        eprintln!("skipped: set RIG_EVALS_AGENTCORE_RUNTIME_ARN to run against AgentCore");
        return;
    };
    let agentcore = AgentCore::from_env().await;
    let plain = " Respondé solo con el número, sin separadores de miles.";
    let dataset = Dataset::new([
        Case::new(
            "multiply",
            format!("¿Cuánto es 987 * 654? Usá calculate.{plain}"),
        )
        .expected("645498")
        .trajectory([rig_evals::ExpectedCall::new("calculate")])
        .assertion("The agent computes the product with the calculate tool"),
        Case::new(
            "screws",
            format!(
                "Una caja trae 48 tornillos. Compro 23 cajas y uso 379 tornillos. \
                 ¿Cuántos tornillos me quedan? Usá calculate.{plain}"
            ),
        )
        .expected("725"),
    ]);
    let report = Experiment::new("agentcore-live")
        .dataset(dataset)
        .target(agentcore.target(arn))
        .evaluator(agentcore.evaluator("Builtin.Correctness"))
        .evaluator(agentcore.evaluator("Builtin.Helpfulness"))
        .evaluator(ContainsExpected)
        .run()
        .await
        .expect("the experiment runs");
    println!("{}", report.to_markdown());

    for result in &report.trials {
        let trial = &result.trial;
        println!("{}: {:?}", trial.case_id, trial.output);
        for score in &result.scores {
            println!(
                "  {} = {:.2} {:?}: {}",
                score.evaluator,
                score.value,
                score.label.as_deref().unwrap_or_default(),
                score.explanation.as_deref().unwrap_or_default()
            );
        }
        assert_eq!(trial.error, None, "{} failed", trial.case_id);
        assert!(
            trial.session_id.is_some(),
            "{} has no session",
            trial.case_id
        );
        for evaluator in ["Builtin.Correctness", "Builtin.Helpfulness"] {
            let score = result
                .scores
                .iter()
                .find(|score| score.evaluator == evaluator)
                .expect("a score per evaluator");
            assert_ne!(
                score.label.as_deref(),
                Some("error"),
                "{evaluator} on {}: {:?}",
                trial.case_id,
                score.explanation
            );
        }
    }
}
