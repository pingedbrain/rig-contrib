//! The Swarm, streaming and Graph snippets of the README, run end to end.
//!
//! ```sh
//! # No key: scripted models. Prints the handoff, the streamed run and the
//! # graph join, and checks the join before it exits.
//! cargo run -p rig-multiagent --example swarm_graph
//!
//! # Live: every agent calls a model on OpenRouter. Set the model to use; the
//! # key is read from OPENROUTER_API_KEY and never printed.
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=deepseek/deepseek-chat \
//!     cargo run -p rig-multiagent --example swarm_graph
//!
//! # Live on OpenCode Go instead: RIG_MODEL=opencode-go/<model>, with the key in
//! # OPENCODE_GO_API_KEY (or OPENCODE_API_KEY), never printed.
//! RIG_MODEL=opencode-go/glm-5.3 OPENCODE_GO_API_KEY=... \
//!     cargo run -p rig-multiagent --example swarm_graph
//! ```
//!
//! Live mode is chosen by `OPENROUTER_MODEL` or `RIG_MODEL`, not by the key
//! alone, so a key in the environment does not start paid calls by accident.
//! Live replies are capped at 4096 tokens. Reasoning tokens count toward the
//! cap, so a reasoning model cut at a lower cap stops with `finish_reason`
//! `length`. Set `RIG_MAX_TOKENS` to change the cap. The scripted run keeps its
//! 1024-token cap.
//! The scripted answers come from Rig's mock models, as the crate's tests use
//! them; in live
//! mode the model decides what each agent says, so only the scripted run
//! checks the join. A live model may also keep handing off instead of
//! answering, so a live run can stop at `max_handoffs` with an error.
//! A failed run prints only the failure's kind and exits 1: a Rig error's text
//! can carry provider output, so it is not printed.

use std::error::Error;
use std::process::ExitCode;

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::DynModel;
use rig_core::completion::CompletionRequest;
use rig_core::operation::Completion;
use rig_core::providers::openai::OpenAI;
use rig_core::providers::{opencode, openrouter};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_multiagent::{Graph, GraphFailure, HANDOFF_TOOL, Swarm, SwarmEvent, SwarmFailure};
use serde_json::json;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// Where the agents get their answers.
enum Backend {
    /// Rig's mock models, scripted per agent.
    Scripted,
    /// A model on OpenRouter or OpenCode Go.
    Live {
        client: OpenAI,
        model: String,
        max_tokens: u64,
    },
}

/// The reply cap for the scripted run, as before.
const SCRIPTED_MAX_TOKENS: u64 = 1024;
/// The reply cap for live runs. Reasoning tokens count toward it.
const LIVE_MAX_TOKENS: u64 = 4096;

impl Backend {
    fn from_env() -> Result<Self> {
        // RIG_MODEL=opencode-go/<model> picks OpenCode Go; any other value
        // is ignored here and OPENROUTER_MODEL decides, as before.
        if let Ok(model) = std::env::var("RIG_MODEL")
            && let Some(name) = model.strip_prefix("opencode-go/")
        {
            if name.is_empty() {
                return Err("RIG_MODEL is opencode-go/ with no model name".into());
            }
            let key = std::env::var("OPENCODE_GO_API_KEY")
                .or_else(|_| std::env::var("OPENCODE_API_KEY"))?;
            return Ok(Self::Live {
                client: opencode::go_new(key),
                model: name.to_owned(),
                max_tokens: live_max_tokens()?,
            });
        }
        match std::env::var("OPENROUTER_MODEL") {
            Ok(model) if !model.is_empty() => Ok(Self::Live {
                client: openrouter::from_env()?,
                model,
                max_tokens: live_max_tokens()?,
            }),
            _ => Ok(Self::Scripted),
        }
    }

    fn max_tokens(&self) -> u64 {
        match self {
            Self::Scripted => SCRIPTED_MAX_TOKENS,
            Self::Live { max_tokens, .. } => *max_tokens,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Scripted => "scripted models".into(),
            Self::Live { model, .. } => format!("live model {model}"),
        }
    }
}

/// `RIG_MAX_TOKENS` if set, else the live default.
fn live_max_tokens() -> Result<u64> {
    match std::env::var("RIG_MAX_TOKENS") {
        Ok(value) => value
            .parse()
            .map_err(|error| format!("RIG_MAX_TOKENS: {error}").into()),
        Err(_) => Ok(LIVE_MAX_TOKENS),
    }
}

/// An agent for a non-streamed run. In scripted mode its model is returned
/// too, so the caller can read what the model was sent.
fn agent(
    backend: &Backend,
    name: &str,
    description: &str,
    turns: Vec<MockTurn>,
) -> (Agent, Option<MockCompletionModel>) {
    match backend {
        Backend::Scripted => {
            let model = MockCompletionModel::from_turns(turns);
            (
                build(model.clone(), name, description, backend.max_tokens()),
                Some(model),
            )
        }
        Backend::Live { client, model, .. } => {
            let _ = turns; // the live model writes its own turns
            (
                build(
                    client.completion(model),
                    name,
                    description,
                    backend.max_tokens(),
                ),
                None,
            )
        }
    }
}

/// An agent for a streamed run.
fn streaming_agent(
    backend: &Backend,
    name: &str,
    description: &str,
    turns: Vec<Vec<MockStreamEvent>>,
) -> Agent {
    match backend {
        Backend::Scripted => {
            let turns = turns.into_iter().map(|mut turn| {
                turn.push(MockStreamEvent::final_response_with_default_usage());
                turn
            });
            let model = MockCompletionModel::from_stream_turns(turns);
            build(model, name, description, backend.max_tokens())
        }
        Backend::Live { client, model, .. } => {
            let _ = turns; // the live model writes its own turns
            build(
                client.completion(model),
                name,
                description,
                backend.max_tokens(),
            )
        }
    }
}

fn build(
    model: impl Into<DynModel<Completion>>,
    name: &str,
    description: &str,
    max_tokens: u64,
) -> Agent {
    AgentBuilder::new(model)
        .name(name)
        .description(description)
        .max_tokens(max_tokens)
        .build()
}

fn handoff(to: &str, message: &str) -> MockTurn {
    MockTurn::tool_call(
        "call-1",
        HANDOFF_TOOL,
        json!({"agent_name": to, "message": message}),
    )
}

fn handoff_event(to: &str, message: &str) -> MockStreamEvent {
    MockStreamEvent::tool_call(
        "call-1",
        HANDOFF_TOOL,
        json!({"agent_name": to, "message": message}),
    )
}

/// The short name of a swarm failure, the only part of it that is printed.
fn swarm_kind(reason: &SwarmFailure) -> &'static str {
    match reason {
        SwarmFailure::Agent { .. } => "agent_failed",
        SwarmFailure::Timeout { .. } => "timeout",
        SwarmFailure::MaxHandoffs { .. } => "max_handoffs",
        SwarmFailure::Repetitive { .. } => "repetitive_handoffs",
        SwarmFailure::Checkpoint { .. } => "checkpoint",
        _ => "other",
    }
}

/// The short name of a graph failure, the only part of it that is printed.
fn graph_kind(failure: &GraphFailure) -> &'static str {
    match failure {
        GraphFailure::Node { .. } => "node_failed",
        GraphFailure::NodeTimeout { .. } => "node_timeout",
        GraphFailure::Timeout => "timeout",
        GraphFailure::MaxNodeExecutions { .. } => "max_node_executions",
        GraphFailure::Checkpoint { .. } => "checkpoint",
        _ => "other",
    }
}

fn check(ok: bool, what: &str) -> Result {
    if ok {
        Ok(())
    } else {
        Err(format!("check failed: {what}").into())
    }
}

/// Everything a model was sent, as JSON text to search.
fn sent(request: &CompletionRequest) -> Result<String> {
    Ok(serde_json::to_string(&request.chat_history)?)
}

/// The Swarm snippet: a researcher hands the writing to a writer.
async fn swarm_run(backend: &Backend) -> Result {
    println!("== Swarm::run ({})", backend.label());
    let (explorer, _) = agent(
        backend,
        "explorer",
        "Reads the project, then hands the summary to the writer",
        vec![handoff("writer", "Write a two-sentence summary.")],
    );
    let (writer, _) = agent(
        backend,
        "writer",
        "Writes the final answer; it always answers and never hands off",
        vec![MockTurn::text(
            "It is a multi-agent crate for Rig: swarms and graphs of agents.",
        )],
    );
    let team = Swarm::builder()
        .agent(explorer) // the first agent added is the entry
        .agent(writer)
        .max_handoffs(6)
        .build()?;
    let run = match team.run("What does this project do?").await {
        Ok(run) => run,
        Err(error) => {
            // Live runs can hand off until the limit; show the trail anyway.
            for handoff in &error.handoffs {
                println!(
                    "handoff {} -> {}: {}",
                    handoff.from, handoff.to, handoff.message
                );
            }
            let kind = swarm_kind(&error.reason);
            println!("handoffs: {}, failed: {kind}", error.handoffs.len());
            return Err(format!("swarm run failed: {kind}").into());
        }
    };
    for handoff in &run.handoffs {
        println!(
            "handoff {} -> {}: {}",
            handoff.from, handoff.to, handoff.message
        );
    }
    println!(
        "handoffs: {}, {} answered: {}\n",
        run.handoffs.len(),
        run.agent,
        run.answer
    );
    Ok(())
}

/// The streaming snippet: the same team, reporting as it goes.
async fn swarm_stream(backend: &Backend) -> Result {
    println!("== Swarm::stream ({})", backend.label());
    let explorer = streaming_agent(
        backend,
        "explorer",
        "Reads the project, then hands the summary to the writer",
        vec![vec![
            MockStreamEvent::text("Checking the crate. "),
            handoff_event("writer", "Write a two-sentence summary."),
        ]],
    );
    let writer = streaming_agent(
        backend,
        "writer",
        "Writes the final answer; it always answers and never hands off",
        vec![vec![
            MockStreamEvent::text("It is a multi-agent crate "),
            MockStreamEvent::text("for Rig."),
        ]],
    );
    let team = Swarm::builder().agent(explorer).agent(writer).build()?;

    let mut events = team.stream("What does this project do?");
    let mut answered = false;
    while let Some(event) = events.next().await {
        match event {
            SwarmEvent::AgentStarted { agent } => println!("\n[{agent}]"),
            SwarmEvent::Text { text, .. } => print!("{text}"),
            SwarmEvent::Handoff(handoff) => println!("\n-> {}", handoff.to),
            SwarmEvent::Finished(outcome) => match *outcome {
                Ok(run) => {
                    answered = true;
                    println!("\nanswer by {}: {}", run.agent, run.answer);
                }
                Err(error) => println!("\nfailed: {}", swarm_kind(&error.reason)),
            },
            _ => {}
        }
    }
    check(answered, "the streamed run finished with an answer")?;
    println!();
    Ok(())
}

/// The Graph snippet: explore, then two branches in parallel, joined by a report.
async fn graph_join(backend: &Backend) -> Result {
    println!("== Graph::run ({})", backend.label());
    let (explorer, _) = agent(
        backend,
        "explorer",
        "Reads the file",
        vec![MockTurn::text("src/tools.rs defines three tools.")],
    );
    let (strengths, _) = agent(
        backend,
        "strengths",
        "Lists the strengths",
        vec![MockTurn::text("Strength: small, typed tools.")],
    );
    let (risks, _) = agent(
        backend,
        "risks",
        "Lists the risks",
        vec![MockTurn::text("Risk: no input validation.")],
    );
    let (report, report_model) = agent(
        backend,
        "report",
        "Joins the findings into one report",
        vec![MockTurn::text(
            "Report: small typed tools, but no validation.",
        )],
    );
    let review = Graph::builder()
        .node("explore", explorer)
        .node("strengths", strengths)
        .node("risks", risks)
        .node("report", report)
        .edge("explore", "strengths")
        .edge("explore", "risks")
        .edge("strengths", "report")
        .edge("risks", "report")
        .build()?;

    let run = match review.run("Review src/tools.rs").await {
        Ok(run) => run,
        Err(error) => {
            let kind = error.failures.first().map_or("none", graph_kind);
            return Err(format!("graph run failed: {kind}").into());
        }
    };
    println!("{}", run.answer());

    // The report got both branches' outputs as its input: the join.
    if let Some(model) = report_model {
        let request = model.requests();
        let input = request.first().ok_or("the report made no model call")?;
        let text = sent(input)?;
        check(
            text.contains("From strengths:"),
            "the report sees strengths",
        )?;
        check(text.contains("From risks:"), "the report sees risks")?;
        println!("join check: the report got both branches");
    }
    println!();
    Ok(())
}

async fn run() -> Result {
    let backend = Backend::from_env()?;
    swarm_run(&backend).await?;
    swarm_stream(&backend).await?;
    graph_join(&backend).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
