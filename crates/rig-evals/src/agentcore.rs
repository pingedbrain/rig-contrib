//! AWS Bedrock AgentCore: a deployed runtime as a [`Target`], and AgentCore
//! Evaluations' evaluators as [`Evaluator`]s.
//!
//! The flow of AgentCore's on-demand evaluation: invoke the runtime in a
//! fresh session, read the session's spans from CloudWatch Logs once they
//! arrive, and send them to the `Evaluate` API.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_sdk_bedrockagentcore::types::{
    Context, EvaluationContent, EvaluationExpectedTrajectory, EvaluationInput,
    EvaluationReferenceInput, EvaluationResultContent, SpanContext,
};
use aws_sdk_cloudwatchlogs::types::{QueryStatus, ResultField};
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::operation::BuildError;
use aws_smithy_types::{Blob, Document, Number};
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::{Value, json};

use crate::evaluators::target_failed;
use crate::target::turn_error;
use crate::{Case, EvalError, Evaluator, Score, Target, TargetError, TargetOutput, Trial, Turn};

#[cfg(test)]
mod tests;

/// The Transaction Search log group AgentCore Observability writes spans to.
const SPANS_LOG_GROUP: &str = "aws/spans";

/// How far before a trial's start the span query looks, for clock skew.
const QUERY_MARGIN: Duration = Duration::from_secs(300);

/// The AWS clients AgentCore targets and evaluators share, and the spans
/// downloaded so far, so several evaluators of one run download them once.
#[derive(Debug, Clone)]
pub struct AgentCore {
    runtime: aws_sdk_bedrockagentcore::Client,
    logs: aws_sdk_cloudwatchlogs::Client,
    state: Arc<Mutex<State>>,
}

/// What an experiment learns about its sessions.
// ponytail: grows with the sessions run; one experiment's worth is small.
#[derive(Debug, Default)]
struct State {
    /// Each session's runtime log group, recorded by the target.
    log_groups: HashMap<String, String>,
    /// Each session's spans, once downloaded.
    spans: HashMap<String, Arc<Vec<Value>>>,
}

impl AgentCore {
    /// Clients from the standard AWS configuration: the environment, the
    /// shared config files, an IAM role.
    pub async fn from_env() -> Self {
        Self::new(&aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await)
    }

    /// Clients from `config`.
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        Self {
            runtime: aws_sdk_bedrockagentcore::Client::new(config),
            logs: aws_sdk_cloudwatchlogs::Client::new(config),
            state: Arc::default(),
        }
    }

    /// The runtime `runtime_arn` (an AgentCore Runtime agent speaking the
    /// HTTP protocol) as a target.
    pub fn target(&self, runtime_arn: impl Into<String>) -> AgentCoreTarget {
        AgentCoreTarget {
            agentcore: self.clone(),
            runtime_arn: runtime_arn.into(),
            qualifier: None,
        }
    }

    /// The AgentCore evaluator `evaluator_id`: a built-in such as
    /// `Builtin.Correctness`, or a custom evaluator's id.
    pub fn evaluator(&self, evaluator_id: impl Into<String>) -> AgentCoreEvaluator {
        AgentCoreEvaluator {
            agentcore: self.clone(),
            evaluator_id: evaluator_id.into(),
            threshold: 0.5,
            ingestion_timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(10),
            runtime_log_group: None,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The spans of `session_id`, from the cache or CloudWatch Logs.
    async fn spans(
        &self,
        session_id: &str,
        log_group: &str,
        since: SystemTime,
        turns: usize,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Arc<Vec<Value>>, AgentCoreError> {
        if let Some(spans) = self.state().spans.get(session_id) {
            return Ok(spans.clone());
        }
        let started = Instant::now();
        let mut previous = None;
        let spans = loop {
            let spans = self.query(session_id, log_group, since).await?;
            let last_chance = started.elapsed().saturating_add(poll_interval) > timeout;
            if settled(previous, &spans, turns) || (last_chance && has_invocation(&spans)) {
                break spans;
            }
            if last_chance {
                return Err(AgentCoreError::NoSpans {
                    session_id: session_id.to_owned(),
                    waited: timeout,
                });
            }
            previous = Some(spans.len());
            tokio::time::sleep(poll_interval).await;
        };
        let spans = Arc::new(spans);
        self.state()
            .spans
            .insert(session_id.to_owned(), spans.clone());
        Ok(spans)
    }

    /// One Logs Insights query for the spans of `session_id`.
    async fn query(
        &self,
        session_id: &str,
        log_group: &str,
        since: SystemTime,
    ) -> Result<Vec<Value>, AgentCoreError> {
        let started = self
            .logs
            .start_query()
            .log_group_names(SPANS_LOG_GROUP)
            .log_group_names(log_group)
            .start_time(epoch_seconds(since))
            .end_time(epoch_seconds(SystemTime::now()) + 60)
            .query_string(spans_query(session_id))
            .send()
            .await
            .map_err(|error| AgentCoreError::Query(error.into()))?;
        let query_id = started.query_id().unwrap_or_default();
        loop {
            let results = self
                .logs
                .get_query_results()
                .query_id(query_id)
                .send()
                .await
                .map_err(|error| AgentCoreError::Query(error.into()))?;
            match results.status() {
                Some(QueryStatus::Complete) => return Ok(documents(results.results())),
                Some(QueryStatus::Running | QueryStatus::Scheduled) | None => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Some(status) => return Err(AgentCoreError::QueryEnded(status.clone())),
            }
        }
    }
}

/// A runtime on AgentCore Runtime; see [`AgentCore::target`].
///
/// Each run invokes the runtime with `{"prompt": <input>}` in a new
/// session and answers the reply's `response`. A multi-turn case sends each
/// turn in that same session, in order, so the runtime keeps the history
/// between turns. The trial carries the session id, which
/// [`AgentCoreEvaluator`]s read the run's spans by.
#[derive(Debug, Clone)]
pub struct AgentCoreTarget {
    agentcore: AgentCore,
    runtime_arn: String,
    qualifier: Option<String>,
}

impl AgentCoreTarget {
    /// Invoke this endpoint of the runtime instead of `DEFAULT`.
    #[must_use]
    pub fn qualifier(mut self, qualifier: impl Into<String>) -> Self {
        self.qualifier = Some(qualifier.into());
        self
    }

    /// Invoke the runtime with `prompt` in `session_id`; the reply.
    async fn invoke(&self, session_id: &str, prompt: &str) -> Result<String, AgentCoreError> {
        let output = self
            .agentcore
            .runtime
            .invoke_agent_runtime()
            .agent_runtime_arn(&self.runtime_arn)
            .set_qualifier(self.qualifier.clone())
            .runtime_session_id(session_id)
            .content_type("application/json")
            .accept("application/json")
            .payload(Blob::new(json!({ "prompt": prompt }).to_string()))
            .send()
            .await
            .map_err(|error| AgentCoreError::Invoke(error.into()))?;
        let body = output.response.collect().await?.into_bytes();
        reply(&body)
    }

    /// Run every turn of `case` in one new session.
    async fn converse(&self, case: &Case) -> Result<TargetOutput, TargetError> {
        let log_group = runtime_log_group(
            &self.runtime_arn,
            self.qualifier.as_deref().unwrap_or("DEFAULT"),
        )?;
        // AgentCore requires at least 33 characters.
        let session_id = format!("rig-evals-{}", uuid::Uuid::new_v4());
        self.agentcore
            .state()
            .log_groups
            .insert(session_id.clone(), log_group);
        run_turns(case, &session_id, |session, prompt| async move {
            self.invoke(&session, &prompt).await
        })
        .await
    }
}

/// Send each turn of `case` to `invoke` in `session_id`, in order, and collect
/// the answers. A turn that fails is a [`TurnFailed`](crate::TurnFailed) of a
/// multi-turn case, with the turns before it.
async fn run_turns<F, Fut>(
    case: &Case,
    session_id: &str,
    mut invoke: F,
) -> Result<TargetOutput, TargetError>
where
    F: FnMut(String, String) -> Fut,
    Fut: Future<Output = Result<String, AgentCoreError>>,
{
    let multi = !case.turns.is_empty();
    let mut output = TargetOutput::default().with_session_id(session_id);
    for (number, input) in case.prompts().into_iter().enumerate() {
        match invoke(session_id.to_owned(), input.to_owned()).await {
            Ok(answer) => {
                output.transcript.push(Turn::new(input, answer.clone()));
                output.output = answer;
            }
            Err(error) => return Err(turn_error(multi, number + 1, output, error.into())),
        }
    }
    if !multi {
        output.transcript.clear();
    }
    Ok(output)
}

impl Target for AgentCoreTarget {
    fn run<'a>(&'a self, case: &'a Case) -> WasmBoxedFuture<'a, Result<TargetOutput, TargetError>> {
        Box::pin(async move { self.converse(case).await })
    }
}

/// An AgentCore Evaluations evaluator; see [`AgentCore::evaluator`].
///
/// Scores trials that ran in an AgentCore session (from an
/// [`AgentCoreTarget`], or any trial with a session id and a
/// [`runtime_log_group`](Self::runtime_log_group)): it waits for the
/// session's spans in CloudWatch Logs, sends them to `Evaluate` with the
/// case's references (expected answer, tool trajectory, assertions), and
/// scores the mean of the results; trace-level evaluators give one per trace.
/// Named after the evaluator id.
#[derive(Debug, Clone)]
pub struct AgentCoreEvaluator {
    agentcore: AgentCore,
    evaluator_id: String,
    threshold: f64,
    ingestion_timeout: Duration,
    poll_interval: Duration,
    runtime_log_group: Option<String>,
}

impl AgentCoreEvaluator {
    /// Pass when the mean value is at least `threshold` (0.5 unless set).
    #[must_use]
    pub fn threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// How long to wait for a run's spans to reach CloudWatch Logs (5
    /// minutes unless set).
    #[must_use]
    pub fn ingestion_timeout(mut self, timeout: Duration) -> Self {
        self.ingestion_timeout = timeout;
        self
    }

    /// How often to query for the spans while waiting (10 seconds unless
    /// set).
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The runtime's log group, such as
    /// `/aws/bedrock-agentcore/runtimes/<runtime-id>-DEFAULT`: for trials
    /// that did not come from an [`AgentCoreTarget`] of the same
    /// [`AgentCore`].
    #[must_use]
    pub fn runtime_log_group(mut self, log_group: impl Into<String>) -> Self {
        self.runtime_log_group = Some(log_group.into());
        self
    }

    async fn score(&self, case: &Case, trial: &Trial) -> Result<Score, AgentCoreError> {
        let session_id = trial
            .session_id
            .as_deref()
            .ok_or(AgentCoreError::NoSession)?;
        let log_group = match &self.runtime_log_group {
            Some(log_group) => log_group.clone(),
            None => self
                .agentcore
                .state()
                .log_groups
                .get(session_id)
                .cloned()
                .ok_or_else(|| AgentCoreError::UnknownRuntime(session_id.to_owned()))?,
        };
        let since = SystemTime::now()
            .checked_sub(trial.latency.saturating_add(QUERY_MARGIN))
            .unwrap_or(UNIX_EPOCH);
        let spans = self
            .agentcore
            .spans(
                session_id,
                &log_group,
                since,
                case.prompts().len(),
                self.ingestion_timeout,
                self.poll_interval,
            )
            .await?;
        let references = reference_inputs(case, session_id, last_trace_id(&spans))?;
        let output = self
            .agentcore
            .runtime
            .evaluate()
            .evaluator_id(&self.evaluator_id)
            .evaluation_input(EvaluationInput::SessionSpans(
                spans.iter().map(document).collect(),
            ))
            .set_evaluation_reference_inputs((!references.is_empty()).then_some(references))
            .send()
            .await
            .map_err(|error| AgentCoreError::Evaluate(error.into()))?;
        score(
            &self.evaluator_id,
            output.evaluation_results(),
            self.threshold,
        )
    }
}

impl Evaluator for AgentCoreEvaluator {
    fn name(&self) -> &str {
        &self.evaluator_id
    }

    fn evaluate<'a>(
        &'a self,
        case: &'a Case,
        trial: &'a Trial,
    ) -> WasmBoxedFuture<'a, Result<Score, EvalError>> {
        Box::pin(async move {
            if let Some(score) = target_failed(&self.evaluator_id, trial) {
                return Ok(score);
            }
            Ok(self.score(case, trial).await?)
        })
    }
}

/// Why an AgentCore target or evaluator failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentCoreError {
    /// The runtime ARN has no `runtime/<id>` part.
    #[error("{0:?} is not an AgentCore runtime ARN")]
    InvalidArn(String),
    /// `InvokeAgentRuntime` failed.
    #[error("InvokeAgentRuntime failed: {}", DisplayErrorContext(.0))]
    Invoke(aws_sdk_bedrockagentcore::Error),
    /// The reply body could not be read.
    #[error("cannot read the runtime's reply: {0}")]
    Body(#[from] aws_smithy_types::byte_stream::error::Error),
    /// The reply is not `{"response": "..."}`.
    #[error("the runtime replied without a response: {0}")]
    Reply(String),
    /// The trial has no session id.
    #[error("the trial has no session id; AgentCore evaluators score trials of an AgentCoreTarget")]
    NoSession,
    /// The session's runtime is unknown.
    #[error(
        "no runtime log group for session {0:?}; use the AgentCore of the target or set runtime_log_group"
    )]
    UnknownRuntime(String),
    /// A CloudWatch Logs Insights call failed.
    #[error("CloudWatch Logs Insights failed: {}", DisplayErrorContext(.0))]
    Query(aws_sdk_cloudwatchlogs::Error),
    /// A Logs Insights query ended without results.
    #[error("the CloudWatch Logs Insights query ended {0:?}")]
    QueryEnded(QueryStatus),
    /// The session's spans did not arrive in time.
    #[error("no invoke_agent span of session {session_id:?} reached CloudWatch Logs in {waited:?}")]
    NoSpans {
        /// The session.
        session_id: String,
        /// How long it waited.
        waited: Duration,
    },
    /// An `Evaluate` request could not be built.
    #[error("cannot build the Evaluate request: {0}")]
    Request(#[from] BuildError),
    /// `Evaluate` failed.
    #[error("Evaluate failed: {}", DisplayErrorContext(.0))]
    Evaluate(aws_sdk_bedrockagentcore::Error),
    /// An evaluation result is an error.
    #[error("{evaluator} failed: {code}: {message}")]
    Result {
        /// The evaluator's name.
        evaluator: String,
        /// The error code.
        code: String,
        /// The error message.
        message: String,
    },
    /// `Evaluate` returned no scored result.
    #[error("{0} returned no score")]
    NoScore(String),
}

impl From<AgentCoreError> for EvalError {
    fn from(error: AgentCoreError) -> Self {
        Self::Other(Box::new(error))
    }
}

/// The runtime id in `arn`, the part after `runtime/`.
fn runtime_id(arn: &str) -> Result<&str, AgentCoreError> {
    arn.rsplit_once(":runtime/")
        .map(|(_, id)| id)
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| AgentCoreError::InvalidArn(arn.to_owned()))
}

/// The log group of the runtime `arn`'s endpoint `qualifier`.
fn runtime_log_group(arn: &str, qualifier: &str) -> Result<String, AgentCoreError> {
    Ok(format!(
        "/aws/bedrock-agentcore/runtimes/{}-{qualifier}",
        runtime_id(arn)?
    ))
}

/// The answer in a runtime's reply body.
fn reply(body: &[u8]) -> Result<String, AgentCoreError> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|reply| reply.get("response")?.as_str().map(str::to_owned))
        .ok_or_else(|| AgentCoreError::Reply(String::from_utf8_lossy(body).into_owned()))
}

/// The Logs Insights query for the spans and events of `session_id`.
fn spans_query(session_id: &str) -> String {
    let session_id = session_id.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "fields @timestamp, @message \
         | filter ispresent(scope.name) and ispresent(attributes.session.id) \
         | filter attributes.session.id = \"{session_id}\" \
         | sort @timestamp asc | limit 500"
    )
}

/// The `@message`s of a query's rows that are JSON objects.
fn documents(rows: &[Vec<ResultField>]) -> Vec<Value> {
    rows.iter()
        .flatten()
        .filter(|field| field.field() == Some("@message"))
        .filter_map(ResultField::value)
        .filter(|message| message.starts_with('{'))
        .filter_map(|message| serde_json::from_str(message).ok())
        .collect()
}

/// Whether `span` is an agent invocation.
fn is_invocation(span: &Value) -> bool {
    span.get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name.starts_with("invoke_agent"))
}

fn has_invocation(spans: &[Value]) -> bool {
    invocations(spans) > 0
}

/// How many agent invocations the spans hold.
fn invocations(spans: &[Value]) -> usize {
    spans.iter().filter(|span| is_invocation(span)).count()
}

/// Whether the spans are all in: one invocation per turn arrived, and the
/// count stayed the same since the `previous` query.
fn settled(previous: Option<usize>, spans: &[Value], turns: usize) -> bool {
    invocations(spans) >= turns && previous == Some(spans.len())
}

/// The trace of the last agent invocation: the one an expected answer is
/// compared with.
fn last_trace_id(spans: &[Value]) -> Option<&str> {
    spans
        .iter()
        .rev()
        .filter(|span| is_invocation(span))
        .find_map(|span| span.get("traceId")?.as_str())
}

/// The case's references for `Evaluate`: assertions and the tool trajectory
/// for the session, the expected answer for its last trace. Only what the
/// case has.
fn reference_inputs(
    case: &Case,
    session_id: &str,
    trace_id: Option<&str>,
) -> Result<Vec<EvaluationReferenceInput>, BuildError> {
    let context = |trace_id: Option<&str>| -> Result<Context, BuildError> {
        Ok(Context::SpanContext(
            SpanContext::builder()
                .session_id(session_id)
                .set_trace_id(trace_id.map(str::to_owned))
                .build()?,
        ))
    };
    let mut references = Vec::new();
    if !case.assertions.is_empty() || case.trajectory.is_some() {
        let assertions = case
            .assertions
            .iter()
            .map(|assertion| EvaluationContent::Text(assertion.clone()))
            .collect::<Vec<_>>();
        let trajectory = case.trajectory.as_ref().map(|calls| {
            EvaluationExpectedTrajectory::builder()
                .set_tool_names(Some(calls.iter().map(|call| call.name.clone()).collect()))
                .build()
        });
        references.push(
            EvaluationReferenceInput::builder()
                .context(context(None)?)
                .set_assertions((!assertions.is_empty()).then_some(assertions))
                .set_expected_trajectory(trajectory)
                .build(),
        );
    }
    if let (Some(expected), Some(trace_id)) = (&case.expected, trace_id) {
        references.push(
            EvaluationReferenceInput::builder()
                .context(context(Some(trace_id))?)
                .expected_response(EvaluationContent::Text(expected.clone()))
                .build(),
        );
    }
    Ok(references)
}

/// The score of `results`: their mean value against `threshold`, their
/// labels and explanations joined.
fn score(
    evaluator: &str,
    results: &[EvaluationResultContent],
    threshold: f64,
) -> Result<Score, AgentCoreError> {
    if let Some(failed) = results
        .iter()
        .find(|result| result.error_code().is_some() || result.error_message().is_some())
    {
        return Err(AgentCoreError::Result {
            evaluator: evaluator.to_owned(),
            code: failed.error_code().unwrap_or("error").to_owned(),
            message: failed.error_message().unwrap_or_default().to_owned(),
        });
    }
    let values: Vec<f64> = results.iter().filter_map(|result| result.value()).collect();
    if values.is_empty() {
        return Err(AgentCoreError::NoScore(evaluator.to_owned()));
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let join = |parts: Vec<&str>, separator| (!parts.is_empty()).then(|| parts.join(separator));
    let mut score = Score::new(evaluator, mean, mean >= threshold);
    score.label = join(results.iter().filter_map(|r| r.label()).collect(), "; ");
    score.explanation = join(
        results.iter().filter_map(|r| r.explanation()).collect(),
        "\n\n",
    );
    Ok(score)
}

/// `value` as a Smithy document.
fn document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(number) => Document::Number(if let Some(n) = number.as_u64() {
            Number::PosInt(n)
        } else if let Some(n) = number.as_i64() {
            Number::NegInt(n)
        } else {
            Number::Float(number.as_f64().unwrap_or_default())
        }),
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(document).collect()),
        Value::Object(map) => Document::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), document(value)))
                .collect(),
        ),
    }
}

/// Seconds since the Unix epoch.
fn epoch_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
