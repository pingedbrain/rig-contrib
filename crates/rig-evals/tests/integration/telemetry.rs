//! What an experiment tells an OpenTelemetry backend, exported in memory.

use opentelemetry::Value;
use opentelemetry::trace::Event;
use opentelemetry_sdk::metrics::InMemoryMetricExporterBuilder;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{Case, Evaluator, Experiment, evaluators};

async fn capture<F: Future<Output = ()>>(body: impl FnOnce() -> F) -> Vec<SpanData> {
    // tracing caches whether each callsite is enabled. With one subscriber
    // registered, a callsite first hit on another test's thread (no
    // subscriber) is cached as disabled everywhere; with two, tracing asks
    // the current subscriber at each event.
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let spans = InMemorySpanExporter::default();
    let (telemetry, subscriber) = rig_otel::Telemetry::builder()
        .span_exporter(spans.clone())
        .metric_exporter(InMemoryMetricExporterBuilder::new().build())
        .logs(false)
        .build()
        .expect("telemetry");
    let guard = tracing::subscriber::set_default(subscriber);
    body().await;
    drop(guard);
    telemetry.force_flush().expect("flush");
    spans.get_finished_spans().expect("spans")
}

fn value(attributes: &[opentelemetry::KeyValue], key: &str) -> Option<Value> {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn result<'a>(span: &'a SpanData, evaluator: &str) -> &'a Event {
    span.events
        .iter()
        .find(|event| {
            event.name == "gen_ai.evaluation.result"
                && value(&event.attributes, "gen_ai.evaluation.name")
                    == Some(Value::from(evaluator.to_owned()))
        })
        .unwrap_or_else(|| panic!("a {evaluator} result on {:?}", span.events))
}

#[tokio::test]
async fn each_score_is_a_gen_ai_evaluation_event_on_the_trial_span() {
    let spans = capture(|| async {
        let agent =
            AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::text("Paris")])).build();
        Experiment::new("geo")
            .dataset(vec![Case::new("capital", "Capital of France?")])
            .target(agent)
            .evaluator(evaluators::contains("Paris"))
            .evaluator(evaluators::contains("Lyon").named("lyon"))
            .run()
            .await
            .expect("runs");
    })
    .await;

    let experiment = spans
        .iter()
        .find(|span| span.name == "eval.experiment")
        .expect("an eval.experiment span");
    let trial = spans
        .iter()
        .find(|span| span.name == "eval.trial")
        .expect("an eval.trial span");
    assert_eq!(trial.parent_span_id, experiment.span_context.span_id());
    assert_eq!(
        value(&trial.attributes, "eval.experiment"),
        Some("geo".into())
    );
    assert_eq!(
        value(&trial.attributes, "eval.case"),
        Some("capital".into())
    );
    assert_eq!(
        value(&trial.attributes, "eval.repetition"),
        Some(Value::I64(1))
    );

    let passed = result(trial, "contains");
    assert_eq!(
        value(&passed.attributes, "gen_ai.evaluation.score.value"),
        Some(Value::F64(1.0))
    );
    assert_eq!(
        value(&passed.attributes, "gen_ai.evaluation.score.label"),
        Some("pass".into())
    );
    assert_eq!(
        value(&passed.attributes, "gen_ai.evaluation.explanation"),
        None
    );

    let failed = result(trial, "lyon");
    assert_eq!(
        value(&failed.attributes, "gen_ai.evaluation.score.value"),
        Some(Value::F64(0.0))
    );
    assert_eq!(
        value(&failed.attributes, "gen_ai.evaluation.score.label"),
        Some("fail".into())
    );
    assert_eq!(
        value(&failed.attributes, "gen_ai.evaluation.explanation"),
        Some("missing [\"Lyon\"]".into())
    );

    // The agent's run is traced under the trial it was evaluated in.
    let operations: Vec<String> = spans
        .iter()
        .filter(|span| span.parent_span_id == trial.span_context.span_id())
        .filter_map(|span| value(&span.attributes, "gen_ai.operation.name"))
        .map(|operation| operation.to_string())
        .collect();
    assert!(!operations.is_empty(), "no GenAI span under the trial");
}
