# rig-evals

Evaluations for [Rig](https://github.com/0xPlaygrounds/rig) agents: run an agent on a
dataset of cases, score every run, and fail CI when a pass rate drops.

```rust
use rig_evals::{Dataset, Experiment, JunitFile, LlmJudge, Markdown, evaluators};
use rig_evals::evaluators::TrajectoryMode;

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

## Targets

A `Target` runs a case. Rig agents are targets: the trial gets the answer, the tool calls
from the run's transcript, and the run's token usage. `target_fn` wraps an async
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
[rig-otel](../rig-otel) installed, any OTel backend shows the results on the evaluated
run's trace.

## Providers

Phase 2 adds adapters: AWS Bedrock AgentCore evaluators as remote `Evaluator`s, and
Langfuse scores as a `Sink`.

## Testing

`cargo test --all-features` runs keyless: agents on scripted models, a scripted judge, and
in-memory OpenTelemetry exporters for the evaluation events.
