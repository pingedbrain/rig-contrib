//! The README's Experiment flow on a small support dataset: a target, a
//! judge, evaluators, a threshold and two sinks (Markdown on stdout, JUnit
//! under `target/evals/`). Cases without a reference answer or trajectory
//! score N/A for the evaluators that need one.
//!
//! ```sh
//! # No key: a scripted target and a scripted judge. These are test mocks, not
//! # models: replace `scripted_target` and `scripted_judge` with real ones.
//! cargo run -p rig-evals --example evals
//!
//! # Live: an agent and a judge on OpenRouter. Both variables are needed; the
//! # key is read from OPENROUTER_API_KEY and never printed.
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=deepseek/deepseek-chat \
//!     cargo run -p rig-evals --example evals
//! ```
//!
//! The report goes to stdout; the mode and the JUnit path go to stderr. The
//! JUnit path is relative to the directory `cargo run` is started in (not the
//! package), so it lands in `./target/evals/` there. The exit code is the
//! report's: 1 when an evaluator misses its threshold.

use std::error::Error;

use rig_agent::AgentBuilder;
use rig_agent::tool::{Tool, ToolContext};
use rig_core::providers::openai::OpenAI;
use rig_core::providers::openrouter;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::evaluators::TrajectoryMode;
use rig_evals::{
    Case, Dataset, Experiment, JunitFile, LlmJudge, Markdown, Report, Target, TargetError,
    TargetOutput, ToolCall, evaluators, target_fn,
};
use serde::Deserialize;
use serde_json::{Value, json};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

const DATASET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/evals/support.jsonl");
const JUNIT: &str = "target/evals/junit.xml";
const RUBRIC: &str = "Is the answer polite, correct and on topic?";

/// Where the agent and the judge get their answers.
enum Backend {
    /// Scripted answers and verdicts, no network.
    Scripted,
    /// Agents on an OpenRouter model.
    Live { client: OpenAI, model: String },
}

impl Backend {
    /// Live only when both `OPENROUTER_MODEL` and `OPENROUTER_API_KEY` are set.
    fn from_env() -> Result<Self> {
        let model = std::env::var("OPENROUTER_MODEL").unwrap_or_default();
        let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
        match (model.is_empty(), key.is_empty()) {
            (false, false) => Ok(Self::Live {
                client: openrouter::from_env()?,
                model,
            }),
            (true, true) => Ok(Self::Scripted),
            _ => {
                eprintln!(
                    "note: set both OPENROUTER_MODEL and OPENROUTER_API_KEY for a live run; using scripted models"
                );
                Ok(Self::Scripted)
            }
        }
    }
}

#[derive(Deserialize)]
struct OrderArgs {
    id: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DeskError(String);

/// The order desk's tools, for the live agent.
struct LookupOrder;

impl Tool for LookupOrder {
    const NAME: &'static str = "lookup_order";
    type Error = DeskError;
    type Args = OrderArgs;
    type Output = String;

    fn description(&self) -> String {
        "Look up an order by its number.".to_owned()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "The order number"}},
            "required": ["id"],
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: OrderArgs,
    ) -> std::result::Result<String, DeskError> {
        Ok(format!(
            "Order {} arrived broken and is eligible for a refund.",
            args.id
        ))
    }
}

struct Refund;

impl Tool for Refund {
    const NAME: &'static str = "refund";
    type Error = DeskError;
    type Args = OrderArgs;
    type Output = String;

    fn description(&self) -> String {
        "Issue a refund for an order by its number.".to_owned()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "The order number"}},
            "required": ["id"],
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: OrderArgs,
    ) -> std::result::Result<String, DeskError> {
        Ok(format!("Refund issued for order {}.", args.id))
    }
}

/// Answers each case of `support.jsonl` as a scripted agent would.
fn scripted_target() -> impl Target {
    target_fn(|case: Case| async move {
        let output = match case.id.as_str() {
            "refund" => {
                TargetOutput::new("A refund was issued for order 7.").with_tool_calls(vec![
                    ToolCall::new("lookup_order", json!({"id": 7})),
                    ToolCall::new("refund", json!({"id": 7})),
                ])
            }
            "hours" => TargetOutput::new("We are open 9 to 5, Monday to Friday."),
            "greeting" => TargetOutput::new("Hello! How can I help?"),
            "order9" => TargetOutput::new("Order 9 has shipped.")
                .with_tool_calls(vec![ToolCall::new("lookup_order", json!({"id": 9}))]),
            "cancel" => TargetOutput::new("Cancel under Settings, then Billing."),
            other => return Err(TargetError::from(format!("no scripted answer for {other}"))),
        };
        Ok::<_, TargetError>(output)
    })
}

/// One verdict per case: the judge runs once per trial, in case order.
fn scripted_judge() -> LlmJudge {
    let verdicts = (0..5)
        .map(|_| MockTurn::text(r#"{"score": 0.9, "reasoning": "Polite, correct and on topic."}"#));
    let agent = AgentBuilder::new(MockCompletionModel::from_turns(verdicts)).build();
    LlmJudge::new(agent, RUBRIC).threshold(0.7)
}

/// The experiment, for any target and judge.
fn experiment(target: impl Target + 'static, judge: LlmJudge) -> Result<Experiment> {
    Ok(Experiment::new("support-bot")
        .dataset(Dataset::load(DATASET)?)
        .target(target)
        // Not applicable where a case has no expected answer or trajectory.
        .evaluator(evaluators::equals().not_applicable_without_reference())
        .evaluator(
            evaluators::trajectory(TrajectoryMode::InOrder).not_applicable_without_reference(),
        )
        .evaluator(evaluators::no_error())
        .evaluator(judge)
        .concurrency(1)
        .sink(JunitFile::new(JUNIT))
        .sink(Markdown::stdout()))
}

async fn run(backend: Backend) -> Result<Report> {
    match backend {
        Backend::Scripted => {
            eprintln!("mode: scripted (no key)");
            Ok(experiment(scripted_target(), scripted_judge())?
                .run()
                .await?)
        }
        Backend::Live { client, model } => {
            eprintln!("mode: live, OpenRouter model {model}");
            let agent = AgentBuilder::new(client.completion(&model))
                .preamble("You are the support desk. Use the tools to look up and refund orders.")
                .tool(LookupOrder)
                .tool(Refund)
                .default_max_turns(4)
                .build();
            let judge = LlmJudge::new(AgentBuilder::new(client.completion(&model)).build(), RUBRIC)
                .threshold(0.7);
            Ok(experiment(agent, judge)?.run().await?)
        }
    }
}

#[tokio::main]
async fn main() -> Result {
    let report = run(Backend::from_env()?).await?;
    eprintln!("JUnit written to {JUNIT}");
    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}
