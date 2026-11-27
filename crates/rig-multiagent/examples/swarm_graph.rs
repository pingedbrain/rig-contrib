//! The Swarm, streaming and Graph snippets of the README, run end to end.
//!
//! ```sh
//! # No key: scripted models. Prints the handoff, the streamed run and the
//! # graph join, and checks the join before it exits.
//! cargo run -p rig-multiagent --example swarm_graph
//!
//! # Live: every agent calls a model on OpenRouter. Set the model to use; the
//! # key is read from OPENROUTER_API_KEY and never printed. Replies are capped
//! # at 256 tokens to keep the run cheap.
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=deepseek/deepseek-chat \
//!     cargo run -p rig-multiagent --example swarm_graph
//! ```
//!
//! Live mode is chosen by `OPENROUTER_MODEL`, not by the key alone, so a key in
//! the environment does not start paid calls by accident. The scripted
//! answers come from Rig's mock models, as the crate's tests use them; in live
//! mode the model decides what each agent says, so only the scripted run
//! checks the join.
//!
//! Swarm agents need `default_max_turns`: the handoff is a tool call, and a Rig
//! agent's default of one turn ends before the tool result is used. See
//! `Swarm::agent` in the crate docs.

use std::error::Error;

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::DynModel;
use rig_core::completion::CompletionRequest;
use rig_core::operation::Completion;
use rig_core::providers::openai::OpenAI;
use rig_core::providers::openrouter;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_multiagent::{Graph, HANDOFF_TOOL, Swarm, SwarmEvent};
use serde_json::json;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// Where the agents get their answers.
enum Backend {
    /// Rig's mock models, scripted per agent.
    Scripted,
    /// A model on OpenRouter.
    Live { client: OpenAI, model: String },
}

impl Backend {
    fn from_env() -> Result<Self> {
        match std::env::var("OPENROUTER_MODEL") {
            Ok(model) if !model.is_empty() => Ok(Self::Live {
                client: openrouter::from_env()?,
                model,
            }),
            _ => Ok(Self::Scripted),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Scripted => "scripted models".into(),
            Self::Live { model, .. } => format!("live model {model}"),
        }
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
            (build(model.clone(), name, description), Some(model))
        }
        Backend::Live { client, model } => {
            let _ = turns; // the live model writes its own turns
            (build(client.completion(model), name, description), None)
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
            build(model, name, description)
        }
        Backend::Live { client, model } => {
            let _ = turns; // the live model writes its own turns
            build(client.completion(model), name, description)
        }
    }
}

fn build(model: impl Into<DynModel<Completion>>, name: &str, description: &str) -> Agent {
    AgentBuilder::new(model)
        .name(name)
        .description(description)
        // Swarm agents hand off by tool call: one turn for the call, one
        // more for the next answer. Rig's default of one turn is too few.
        .default_max_turns(4)
        .max_tokens(256)
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
    let run = team.run("What does this project do?").await?;
    for handoff in &run.handoffs {
        println!(
            "handoff {} -> {}: {}",
            handoff.from, handoff.to, handoff.message
        );
    }
    println!("{} answered: {}\n", run.agent, run.answer);
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
                Err(error) => println!("\nfailed: {error}"),
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

    let run = review.run("Review src/tools.rs").await?;
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

#[tokio::main]
async fn main() -> Result {
    let backend = Backend::from_env()?;
    swarm_run(&backend).await?;
    swarm_stream(&backend).await?;
    graph_join(&backend).await?;
    Ok(())
}
