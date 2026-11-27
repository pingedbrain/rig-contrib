# rig-evals

Evaluations for [Rig](https://github.com/0xPlaygrounds/rig) agents: run an agent on a
dataset of cases, score every run, and fail CI when a pass rate drops.

```rust
use rig_agent::Agent;
use rig_evals::evaluators::TrajectoryMode;
use rig_evals::{Dataset, Experiment, JunitFile, LlmJudge, Markdown, evaluators};

async fn support_bot(agent: Agent, judge: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let report = Experiment::new("support-bot")
        .dataset(Dataset::load("evals/support.jsonl")?)
        .target(agent)
        .evaluator(evaluators::trajectory(TrajectoryMode::InOrder))
        .evaluator(evaluators::budget().max_total_tokens(4_000))
        .evaluator(LlmJudge::new(judge, "Is the answer correct and polite?").threshold(0.7))
        .repetitions(5)
        .concurrency(4)
        .threshold("llm_judge", 0.8)
        .sink(JunitFile::new("target/evals/junit.xml"))
        .sink(Markdown::stdout())
        .run()
        .await?;
    std::process::exit(if report.passed() { 0 } else { 1 });
}
```

## Cases

A case has an `id`, an `input`, and the references evaluators check a run against: an
`expected` answer, an expected `trajectory` of tool calls, and `assertions` in plain
language. These are the reference inputs of AWS Bedrock AgentCore evaluations and the
fields of Strands Evals' cases. `Dataset::load` reads `.json` (an array) or `.jsonl`.

```json
{"id": "refund", "input": "Order 7 arrived broken.", "expected": "A refund was issued.",
 "trajectory": [{"name": "lookup_order", "args": {"id": 7}}, {"name": "refund"}],
 "assertions": ["apologizes", "gives the refund amount"]}
```

### Multi-turn cases

A case with `turns` instead of `input` is a conversation: each turn is a user message,
sent in order on one conversation, so a later turn can refer to earlier answers. The
target answers the last turn, and the evaluators score that answer, the tool calls over
all turns, and the usage over all turns. The trial's `transcript` holds each turn and its
answer; the judge sees it too.

```json
{"id": "city", "turns": ["My favourite city is Lisbon.", "I like walking there.",
 "What is my favourite city?"], "expected": "Lisbon"}
```

A failed turn makes the trial fail with `turn <n>: ...` as its error. The trial keeps the
turns that completed, their tool calls and usage; the evaluators see them as usual.

A case sets `input` or `turns`, not both; a dataset without `turns` reads as before. In
Rust, `Case::multi_turn(id, turns)` builds one. An agent target keeps the history itself, a
multi-turn case starting with an empty history (not the agent's memory store). An
AgentCore target sends every turn in one runtime session. A `target_fn` gets the whole
case: loop over `case.prompts()`, and return the last answer with `transcript` set. A
`node_target` runs single-turn cases only.

## Targets

A `Target` runs a case. Rig agents are targets: the trial gets the answer, the tool calls
from the run's transcript with the result each tool returned (`ToolCall::result`, cut to 4 KiB),
and the run's token usage (summed over the turns of a multi-turn case). `target_fn` wraps an async
function. With the `multiagent` feature, `node_target` wraps a rig-multiagent swarm,
graph or node (answer and usage; nodes do not report tool calls).

## Evaluators

An `Evaluator` scores a trial from 0 to 1 and says whether it passed.

| Evaluator | Passes when |
|---|---|
| `contains`, `contains_all` | the answer contains every string |
| `regex` | the answer matches the pattern |
| `equals` | the trimmed answer equals `expected` |
| `json` | the answer is JSON, with the `require`d keys |
| `trajectory` | the tool calls match the trajectory: `Exact`, `InOrder` or `AnyOrder`; given args match as a subset |
| `budget` | output tokens, total tokens and latency stay within limits |
| `no_error` | the target did not fail |
| `LlmJudge` | a judge agent scores the answer against a rubric at least at the threshold |

### LLM-judge presets

`LlmJudge::helpfulness`, `LlmJudge::faithfulness` and `LlmJudge::goal_success` are judges
with a fixed rubric, named `helpfulness`, `faithfulness` and `goal_success`. They share the
judge's prompt, its 0 to 1 score and its threshold (0.5 unless set). Faithfulness checks the
answer against the transcript, the tool calls with their recorded results and the case's
references; goal success reads
every turn of a multi-turn case.

```rust,no_run
use rig_agent::Agent;
use rig_evals::{Dataset, Experiment, LlmJudge};

async fn run(agent: Agent, judge: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let report = Experiment::new("support-bot")
        .dataset(Dataset::load("evals/support.jsonl")?)
        .target(agent)
        .evaluator(LlmJudge::helpfulness(judge.clone()))
        .evaluator(LlmJudge::faithfulness(judge))
        .threshold("faithfulness", 0.9)
        .run()
        .await?;
    println!("passed: {}", report.passed());
    Ok(())
}
```

A failed target run still gets scored, and the built-ins fail it. An evaluator that
errors (a case without `expected`, a judge without a verdict) gives a failed score
labelled `error`; the experiment carries on. `named` renames an evaluator, to use two of
a kind.

## CI

Each evaluator has a threshold: the share of trials it must pass. It is 1 unless set with
`threshold(name, min_pass_rate)`; with repetitions, a lower one tolerates a
nondeterministic agent. `Report::passed()` is false when any evaluator misses its
threshold: use it as the exit code. The JUnit report has one test case per trial and per
threshold, so CI test views show the failures.

## Timeouts

`trial_timeout(limit)` gives each target run and each evaluator call at most `limit`. A
run that takes longer fails its trial with `timed out after <n>s`; the other trials run on.
It is off unless set. The turns of one multi-turn case share the limit, and a timed-out judge's tokens are not counted in the report. It needs a running tokio runtime.

```rust
use std::time::Duration;

use rig_agent::Agent;
use rig_evals::{Dataset, Experiment, evaluators};

async fn checked(dataset: Dataset, agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let report = Experiment::new("support-bot")
        .dataset(dataset)
        .target(agent)
        .evaluator(evaluators::no_error())
        .trial_timeout(Duration::from_secs(60))
        .run()
        .await?;
    std::process::exit(if report.passed() { 0 } else { 1 });
}
```

## Sinks

A `Sink` receives the report. `JsonFile`, `JunitFile` and `Markdown` (a file, such as
`$GITHUB_STEP_SUMMARY`, or stdout) are built in; `Report::to_json`, `to_junit` and
`to_markdown` give the same text. A sink that fails returns its error with the report.

## OpenTelemetry

The experiment is an `eval.experiment` span, and each trial an `eval.trial` span
(`eval.case`, `eval.repetition`) that the agent's own spans nest in. Each score is a
`gen_ai.evaluation.result` event on the trial span, with `gen_ai.evaluation.name`,
`gen_ai.evaluation.score.value`, `gen_ai.evaluation.score.label` (the label, or
`pass`/`fail`) and `gen_ai.evaluation.explanation`: the event of the OpenTelemetry GenAI
semantic conventions (development status). The crate only emits `tracing` data; with
[rig-otel](https://crates.io/crates/rig-otel) installed, any OTel backend shows the results on the evaluated
run's trace.

## Providers

### AWS Bedrock AgentCore

With the `agentcore` feature, an agent deployed on AgentCore Runtime is a target, and
AgentCore Evaluations' evaluators (`Builtin.Correctness`, `Builtin.Helpfulness`,
`Builtin.GoalSuccessRate`, the trajectory matchers, or a custom evaluator id) are
evaluators. They follow AWS's
[on-demand evaluation](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/getting-started-on-demand.html)
flow:

1. `AgentCoreTarget` invokes the runtime (`InvokeAgentRuntime`, HTTP protocol, payload
   `{"prompt": <input>}`) in a fresh session per trial and answers the reply's `response`.
   A multi-turn case sends each turn in that one session, in order. The trial carries the
   session id.
2. `AgentCoreEvaluator` waits for the session's spans in CloudWatch Logs: it queries
   `aws/spans` and the runtime's `/aws/bedrock-agentcore/runtimes/<runtime-id>-<endpoint>`
   group with Logs Insights every 10 seconds, up to 5 minutes, until an `invoke_agent`
   span is in and the span count stops growing.
3. It sends the spans to `Evaluate` with the case's references: `assertions` and the tool
   names of the `trajectory` for the session, `expected` for the last invocation's trace.
   The score is the mean of the results (one per trace for trace-level evaluators), with
   their labels and explanations; it passes at the threshold (0.5 unless set). A result
   that is an error makes an error score.

The spans are downloaded once per session and shared by every AgentCore evaluator of the
same `AgentCore`. The example below needs the `agentcore` feature, so this README is compile-checked only when that feature is enabled.

```rust
use rig_evals::{AgentCore, Dataset, Experiment, Markdown, evaluators};

async fn agentcore_eval() -> Result<(), Box<dyn std::error::Error>> {
    let agentcore = AgentCore::from_env().await; // or AgentCore::new(&sdk_config)
    let _report = Experiment::new("rig-agent")
        .dataset(Dataset::load("evals/math.jsonl")?)
        .target(agentcore.target("arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/<runtime-id>"))
        .evaluator(agentcore.evaluator("Builtin.Correctness").threshold(0.8))
        .evaluator(agentcore.evaluator("Builtin.Helpfulness"))
        .evaluator(evaluators::no_error())
        .sink(Markdown::stdout())
        .run()
        .await?;
    Ok(())
}
```

The agent must leave spans AgentCore Evaluations can read: deploy it with
[rig-agentcore](https://crates.io/crates/rig-agentcore), export with [rig-otel](https://crates.io/crates/rig-otel)'s `aws` feature,
and record content (`record_content(true)`), so the spans carry the prompts and answers.
Transaction Search must be on in the account.

The caller needs `bedrock-agentcore:InvokeAgentRuntime` on the runtime,
`bedrock-agentcore:Evaluate`, and `logs:StartQuery` and `logs:GetQueryResults` on both log
groups. `.ingestion_timeout`, `.poll_interval` and `.runtime_log_group` (for trials of
another target, given a session id) tune the evaluator; `.qualifier` picks another
runtime endpoint.

Langfuse scores as a `Sink` come next.

## Testing

`cargo test --all-features` runs keyless: agents on scripted models, a scripted judge, and
in-memory OpenTelemetry exporters for the evaluation events. The AgentCore suite runs
against a real runtime only when `RIG_EVALS_AGENTCORE_RUNTIME_ARN` is set (with AWS
credentials and region), and skips otherwise.
