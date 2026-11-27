//! Evaluations for [Rig](https://github.com/0xPlaygrounds/rig) agents.
//!
//! An [`Experiment`] runs a [`Target`] (a Rig agent, an async function, or,
//! with the `multiagent` feature, a swarm or graph) on every [`Case`] of a
//! [`Dataset`], scores each run with [`Evaluator`]s, and hands the
//! [`Report`] to [`Sink`]s.
//!
//! ```no_run
//! use rig_evals::{Dataset, Experiment, JunitFile, evaluators};
//! # async fn run(agent: rig_agent::Agent) -> Result<(), Box<dyn std::error::Error>> {
//! let report = Experiment::new("support-bot")
//!     .dataset(Dataset::load("evals/cases.jsonl")?)
//!     .target(agent)
//!     .evaluator(evaluators::contains("refund"))
//!     .evaluator(evaluators::no_error())
//!     .repetitions(3)
//!     .threshold("contains", 0.8)
//!     .sink(JunitFile::new("target/evals/junit.xml"))
//!     .run()
//!     .await?;
//! std::process::exit(if report.passed() { 0 } else { 1 });
//! # }
//! ```
//!
//! With the `agentcore` feature, `AgentCore` makes a runtime on AWS Bedrock
//! AgentCore a target, and AgentCore Evaluations' evaluators evaluators.
//!
//! Each run is a `tracing` span `eval.trial` inside an `eval.experiment` span,
//! and each score an OpenTelemetry GenAI `gen_ai.evaluation.result` event on
//! it, so a tracing exporter (such as rig-otel) puts the results on the
//! evaluated agent's trace.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::unreachable
    )
)]

#[cfg(feature = "agentcore")]
mod agentcore;
mod case;
mod evaluator;
pub mod evaluators;
mod experiment;
mod report;
mod sink;
mod target;

#[cfg(feature = "agentcore")]
pub use agentcore::{AgentCore, AgentCoreError, AgentCoreEvaluator, AgentCoreTarget};
pub use case::{Case, Dataset, DatasetError, ExpectedCall};
pub use evaluator::{EvalError, Evaluator, Named, Score};
pub use evaluators::{LlmJudge, Verdict};
pub use experiment::{Experiment, ExperimentError};
pub use report::{
    EvaluatorSummary, LatencySummary, Report, ThresholdResult, Tokens, Trial, TrialResult,
};
pub use sink::{JsonFile, JunitFile, Markdown, Sink, SinkError};
pub use target::{
    FnTarget, Target, TargetError, TargetOutput, ToolCall, Turn, TurnFailed, target_fn,
};
#[cfg(feature = "multiagent")]
pub use target::{NodeTarget, node_target};

// The README's AgentCore example needs the `agentcore` feature, so the
// README is compile-checked only with it enabled (CI uses --all-features).
#[cfg(all(doctest, feature = "agentcore"))]
#[doc = include_str!("../README.md")]
mod readme_doctests {}
