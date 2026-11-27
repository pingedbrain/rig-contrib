# Changelog

## Unreleased

- Experiment comparison: `Report::compare(&baseline)` on the candidate report returns a `Comparison`. It gives, per evaluator, the pass-rate and mean deltas (`EvaluatorDelta`); the regressions and fixes, matched by case id (`CaseChange`; a case passes when all its trials pass); the cases only one report has; and the token and latency deltas. `Comparison::to_markdown` and `to_json` render it. Reports already deserialize, so a saved baseline loads with `serde_json::from_str::<Report>`.
- Dataset errors can name the file: `Dataset::load_named(path)` reads a `.json` or `.jsonl` file like `Dataset::load`, but a parse error is `DatasetError::InFile { path, source }`, whose source is the `Line` error (JSON Lines, with the line number) or the new `Element` error (a JSON array element, with its index from 0), or `Json` for a file that is not an array. `Dataset::load`, `from_json` and `from_jsonl` return the same errors as before. A case id that repeats is logged as a warning at load; loading still accepts it, and `Experiment::run` still refuses it.
- Docs: the front page and README give the case file shape (a field table and a three-case JSONL sample, single-turn and multi-turn), and note content recording for the OpenTelemetry output.
- Trial timeout: `Experiment::trial_timeout(Duration)` is opt-in (off by default). A target run over the limit fails its trial with `timed out after <n>s` (an agent keeps its completed turns, as a failed turn does); an evaluator call over it is a failed score labelled `error`. Other trials run on. The limit is per trial and per call, and the turns of one case share it. A timed-out judge's token usage is dropped, so the report's judge tokens undercount. It needs a running tokio runtime. A limit too large for the clock is no limit.
- New runtime dependency: `rig-evals` now depends directly on tokio (features `rt`, `sync`, `time`) for every user, not only with `agentcore`, where it was optional before. The dependency is required, not transitive.
- README snippets are compile-checked as doctests (the AgentCore example with the `agentcore` feature).
- Tool results: `ToolCall` has a `result` field, the text the tool returned (cut to 4 KiB, marked `[truncated]`), filled by the agent target from the matching tool result (by call id). The faithfulness judge sees the results. `result` is left out of JSON when absent, so existing JSON is unchanged; AgentCore still records no tool calls.

## 0.47.3 (2026-11-27)

- Multi-turn cases: a case with `turns` (a list of user messages) instead of `input` is sent
  turn by turn on one conversation. The agent target keeps the history between turns, the
  AgentCore target reuses one runtime session, and the evaluators score the last answer with the
  tool calls and usage of all turns. `Trial::transcript` (and `Turn`) holds each turn and its
  answer. A failed turn is an error prefixed `turn <n>:`; the trial keeps the completed turns, their
  tool calls and usage (`TurnFailed` carries them). AgentCore waits for one invocation span per
  turn before scoring. Single-turn datasets are unchanged; a case that sets both `input` and
  `turns` is an `ExperimentError::InputAndTurns`. `Case::multi_turn` and `Case::prompts` are new.
- Example: `cargo run -p rig-evals --example evals` runs a mixed support dataset (N/A where a case has no reference), keyless with scripted models, or on OpenRouter when `OPENROUTER_MODEL` and `OPENROUTER_API_KEY` are both set. It prints the Markdown report and writes JUnit to `target/evals/junit.xml`.
- Not applicable: `Score::not_applicable` marks a score the evaluator could not apply (N/A). It is
  left out of pass rates and means, shown as N/A in Markdown, as `not_applicable` in JSON
  (`EvaluatorSummary::not_applicable` counts them) and as a skipped JUnit test case (the
  testsuite's `skipped` attribute counts them). `Equals::not_applicable_without_reference()` and
  `Trajectory::not_applicable_without_reference()` opt in to it for cases lacking the reference.
  The default is unchanged: such a case still fails with `MissingReference`.
- Judge tokens: an `LlmJudge` records its usage on its score (`Score::usage`), failed judgments
  included. The report's `tokens` now include the judges' tokens (a change to the totals of reports
  with a judge), and `Report::judge_tokens` gives the judges' share.
- LLM-judge presets: `LlmJudge::helpfulness`, `LlmJudge::faithfulness` and `LlmJudge::goal_success` are judges with a fixed rubric, named `helpfulness`, `faithfulness` and `goal_success`, on the same prompt, score and threshold as `LlmJudge::new` (which is unchanged, still named `llm_judge`).
- A judge that returns no verdict, or whose reply is cut at `max_tokens`, is a failed score labelled
  `error` (as an evaluator error is), not an `EvalError`. The explanation says the reply was
  truncated at max_tokens when it was.

Versions follow Rig's: rig-evals 0.47.x works with Rig 0.47.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.
- Links between the crates' READMEs point to crates.io, so they work there.

## 0.47.0 (2026-11-26)

First release.

- `Case` and `Dataset`: inputs with an expected answer, an expected tool trajectory and
  assertions; read from JSON or JSON Lines.
- `Target`: Rig agents (answer, tool calls from the transcript, usage), async functions
  (`target_fn`), and rig-multiagent nodes (`node_target`, feature `multiagent`).
- `Evaluator` and `Score`; built-in `contains`, `regex`, `equals`, `json`, `trajectory`
  (exact, in order, any order), `budget`, `no_error`, and `LlmJudge` (a judge model, a
  rubric and a threshold).
- `Experiment`: repetitions, bounded concurrency, per-evaluator pass-rate thresholds; a
  failing target or evaluator becomes a failed score, not an aborted run.
- `Report`: per-evaluator pass rate and mean, tokens, latency p50 and p95, thresholds,
  `passed()` for CI; as JSON, JUnit XML or Markdown, through the `JsonFile`, `JunitFile`
  and `Markdown` sinks.
- AWS Bedrock AgentCore (feature `agentcore`): `AgentCoreTarget` invokes a runtime in a
  fresh session per trial; `AgentCoreEvaluator` reads the session's spans from CloudWatch
  Logs and scores them with AgentCore Evaluations (built-in or custom evaluators), with
  the case's references. `Trial` and `TargetOutput` carry an optional `session_id`.
- `eval.experiment` and `eval.trial` spans, and a `gen_ai.evaluation.result` event per
  score (OpenTelemetry GenAI semantic conventions).
