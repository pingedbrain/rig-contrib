# Changelog

## Unreleased

- Not applicable: `Score::not_applicable` marks a score the evaluator could not apply (N/A). It is
  left out of pass rates and means, shown as N/A in Markdown, as `not_applicable` in JSON and as a
  skipped test case in JUnit. `Equals::not_applicable_without_reference()` and
  `Trajectory::not_applicable_without_reference()` opt in to it for cases lacking the reference.
  The default is unchanged: such a case still fails with `MissingReference`.
- Judge tokens: an `LlmJudge` records its usage on its score (`Score::usage`). The report's `tokens`
  now include the judges' tokens (a change to the totals of reports with a judge), and
  `Report::judge_tokens` gives the judges' share.
- A judge reply cut at `max_tokens` fails with `EvalError::JudgeTruncated`, which says so.
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
