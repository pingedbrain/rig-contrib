//! Running a target on a dataset and scoring every run.

use std::collections::HashSet;
use std::time::Instant;

use futures::StreamExt;
use tracing::Instrument;

use crate::evaluator::clamp;
use crate::{Case, Dataset, Evaluator, Report, Score, Sink, SinkError, Target, Trial, TrialResult};

/// An evaluation: a target run on every case of a dataset, a number of
/// times, each run scored by every evaluator; the report goes to the sinks.
///
/// Each run is an `eval.trial` span (fields `eval.experiment`, `eval.case`,
/// `eval.repetition`) inside an `eval.experiment` span, and the target's own
/// spans (an agent's `invoke_agent`) nest in it. After scoring, each score is
/// a `gen_ai.evaluation.result` event on the trial span, with the
/// OpenTelemetry GenAI fields `gen_ai.evaluation.name`,
/// `gen_ai.evaluation.score.value`, `gen_ai.evaluation.score.label` (the
/// score's label, else `pass` or `fail`) and `gen_ai.evaluation.explanation`.
pub struct Experiment {
    name: String,
    dataset: Dataset,
    target: Option<Box<dyn Target>>,
    evaluators: Vec<Box<dyn Evaluator>>,
    repetitions: usize,
    concurrency: usize,
    thresholds: Vec<(String, f64)>,
    sinks: Vec<Box<dyn Sink>>,
}

impl Experiment {
    /// An experiment named `name`: one repetition, four runs at a time.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            dataset: Dataset::default(),
            target: None,
            evaluators: Vec::new(),
            repetitions: 1,
            concurrency: 4,
            thresholds: Vec::new(),
            sinks: Vec::new(),
        }
    }

    /// The cases to run.
    #[must_use]
    pub fn dataset(mut self, dataset: impl Into<Dataset>) -> Self {
        self.dataset = dataset.into();
        self
    }

    /// What to run.
    #[must_use]
    pub fn target(mut self, target: impl Target + 'static) -> Self {
        self.target = Some(Box::new(target));
        self
    }

    /// Add an evaluator. Each one scores every run.
    #[must_use]
    pub fn evaluator(mut self, evaluator: impl Evaluator + 'static) -> Self {
        self.evaluators.push(Box::new(evaluator));
        self
    }

    /// Run each case `repetitions` times, to measure how often a
    /// nondeterministic target passes.
    #[must_use]
    pub fn repetitions(mut self, repetitions: usize) -> Self {
        self.repetitions = repetitions;
        self
    }

    /// Run at most `concurrency` trials at a time (0 counts as 1).
    #[must_use]
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// The report passes only if `evaluator` passes at least `min_pass_rate`
    /// (0 to 1) of the runs. Without one, an evaluator must pass every run.
    #[must_use]
    pub fn threshold(mut self, evaluator: impl Into<String>, min_pass_rate: f64) -> Self {
        self.thresholds.push((evaluator.into(), min_pass_rate));
        self
    }

    /// Add a sink for the report.
    #[must_use]
    pub fn sink(mut self, sink: impl Sink + 'static) -> Self {
        self.sinks.push(Box::new(sink));
        self
    }

    /// Run every case, score every run, and hand the report to the sinks.
    ///
    /// A target that fails makes a trial with an
    /// [`error`](crate::Trial::error); an evaluator that fails makes a failed
    /// score labelled `error`. Neither stops the experiment.
    pub async fn run(&self) -> Result<Report, ExperimentError> {
        let target = self.check()?;
        let span = tracing::info_span!("eval.experiment", eval.experiment = %self.name);
        // Lazy: a trial starts when buffer_unordered polls it.
        let mut runs = Vec::with_capacity(self.dataset.cases.len() * self.repetitions);
        for (index, case) in self.dataset.cases.iter().enumerate() {
            for repetition in 1..=self.repetitions {
                runs.push(self.trial(target, index, case, repetition, &span));
            }
        }
        let mut trials: Vec<(usize, TrialResult)> = futures::stream::iter(runs)
            .buffer_unordered(self.concurrency.max(1))
            .collect()
            .instrument(span.clone())
            .await;
        trials.sort_by_key(|(index, result)| (*index, result.trial.repetition));
        let evaluators: Vec<String> = self
            .evaluators
            .iter()
            .map(|e| e.name().to_owned())
            .collect();
        let report = Report::new(
            self.name.clone(),
            self.repetitions,
            trials.into_iter().map(|(_, result)| result).collect(),
            &evaluators,
            &self.thresholds,
        );
        for sink in &self.sinks {
            if let Err(source) = sink.record(&report).await {
                return Err(ExperimentError::Sink {
                    source,
                    report: Box::new(report),
                });
            }
        }
        Ok(report)
    }

    /// The target, once the experiment is known to make sense.
    fn check(&self) -> Result<&dyn Target, ExperimentError> {
        let target = self.target.as_deref().ok_or(ExperimentError::NoTarget)?;
        if self.dataset.cases.is_empty() {
            return Err(ExperimentError::EmptyDataset);
        }
        if self.evaluators.is_empty() {
            return Err(ExperimentError::NoEvaluators);
        }
        if self.repetitions == 0 {
            return Err(ExperimentError::NoRepetitions);
        }
        let mut seen = HashSet::new();
        if let Some(case) = self
            .dataset
            .cases
            .iter()
            .find(|case| !seen.insert(&case.id))
        {
            return Err(ExperimentError::DuplicateCase(case.id.clone()));
        }
        let mut seen = HashSet::new();
        if let Some(evaluator) = self.evaluators.iter().find(|e| !seen.insert(e.name())) {
            return Err(ExperimentError::DuplicateEvaluator(
                evaluator.name().to_owned(),
            ));
        }
        for (evaluator, min) in &self.thresholds {
            if !seen.contains(evaluator.as_str()) {
                return Err(ExperimentError::UnknownThreshold(evaluator.clone()));
            }
            if !(0.0..=1.0).contains(min) {
                return Err(ExperimentError::InvalidThreshold {
                    evaluator: evaluator.clone(),
                    min_pass_rate: *min,
                });
            }
        }
        Ok(target)
    }

    /// Run `case` (number `index` in the dataset) once and score the run, in
    /// an `eval.trial` span.
    async fn trial(
        &self,
        target: &dyn Target,
        index: usize,
        case: &Case,
        repetition: usize,
        parent: &tracing::Span,
    ) -> (usize, TrialResult) {
        let span = tracing::info_span!(
            parent: parent,
            "eval.trial",
            eval.experiment = %self.name,
            eval.case = %case.id,
            eval.repetition = repetition as i64,
        );
        async move {
            let started = Instant::now();
            let ran = target.run(case).await;
            let mut trial = Trial::new(case.id.clone(), repetition, "");
            trial.latency = started.elapsed();
            match ran {
                Ok(output) => {
                    trial.output = output.output;
                    trial.tool_calls = output.tool_calls;
                    trial.usage = output.usage;
                }
                Err(error) => trial.error = Some(error.to_string()),
            }
            let mut scores = Vec::with_capacity(self.evaluators.len());
            for evaluator in &self.evaluators {
                let name = evaluator.name();
                let mut score = evaluator
                    .evaluate(case, &trial)
                    .await
                    .unwrap_or_else(|error| {
                        Score::fail(name, error.to_string()).with_label("error")
                    });
                score.evaluator = name.to_owned();
                score.value = clamp(score.value);
                let label =
                    score
                        .label
                        .as_deref()
                        .unwrap_or(if score.passed { "pass" } else { "fail" });
                tracing::info!(
                    gen_ai.evaluation.name = %score.evaluator,
                    gen_ai.evaluation.score.value = score.value,
                    gen_ai.evaluation.score.label = %label,
                    gen_ai.evaluation.explanation = score.explanation.as_deref(),
                    "gen_ai.evaluation.result"
                );
                scores.push(score);
            }
            (index, TrialResult { trial, scores })
        }
        .instrument(span)
        .await
    }
}

/// Why an experiment could not run, or could not deliver its report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExperimentError {
    /// No target was set.
    #[error("the experiment has no target")]
    NoTarget,
    /// The dataset has no cases.
    #[error("the dataset has no cases")]
    EmptyDataset,
    /// No evaluator was added.
    #[error("the experiment has no evaluators")]
    NoEvaluators,
    /// Repetitions set to 0.
    #[error("repetitions must be at least 1")]
    NoRepetitions,
    /// Two cases share an id.
    #[error("two cases have the id {0:?}")]
    DuplicateCase(String),
    /// Two evaluators share a name; rename one with `named`.
    #[error("two evaluators are named {0:?}; rename one with named()")]
    DuplicateEvaluator(String),
    /// A threshold names no evaluator of the experiment.
    #[error("a threshold names {0:?}, which is not an evaluator of the experiment")]
    UnknownThreshold(String),
    /// A threshold outside 0..=1.
    #[error("the threshold of {evaluator:?} is {min_pass_rate}; it must be from 0 to 1")]
    InvalidThreshold {
        /// The evaluator.
        evaluator: String,
        /// The threshold given.
        min_pass_rate: f64,
    },
    /// A sink failed. The experiment ran: here is its report.
    #[error("a sink failed: {source}")]
    Sink {
        /// The sink's error.
        source: SinkError,
        /// The report.
        report: Box<Report>,
    },
}
