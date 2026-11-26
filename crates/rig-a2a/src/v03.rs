//! A2A v0.3 compatibility, for peers that have not moved to v1.0 yet (such as
//! Strands Agents 1.x, which pins `a2a-sdk<0.4`).
//!
//! Serving: [`router`] answers v0.3 JSON-RPC (`message/send`,
//! `message/stream`, `tasks/get`, `tasks/cancel`) by translating each call to
//! the v1.0 request handler the v1.0 bindings use, so both versions share one
//! task store and one conversation history. The server publishes a v0.3 agent
//! card ([`card`]) under the same prefix, so a v0.3 client is pointed at
//! `{base}/v03`.
//!
//! Calling: [`send`] and [`stream`] post `message/send` and `message/stream`
//! to a v0.3 agent and read the answers as v1.0 events.
//!
//! The v0.3 wire shapes differ from v1.0 in names, not meaning: parts carry a
//! `kind` (`text`, `file`, `data`), states and roles are lowercase words, and
//! every object names its `kind`.

use std::sync::Arc;

use a2a::{
    A2AError, AgentCard, Artifact, CancelTaskRequest, GetTaskRequest, Message, Part, PartContent,
    Role, SendMessageConfiguration, SendMessageRequest, SendMessageResponse, StreamResponse, Task,
    TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use a2a_server::{DefaultRequestHandler, RequestHandler, ServiceParams};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

/// Where the v0.3 surface is mounted under the server's base URL.
pub const V03_PATH: &str = "/v03";

const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;

// ---------------------------------------------------------------- serving --

/// The v0.3 JSON-RPC endpoint, at `/` relative to where it is nested. The
/// v0.3 card is routed by the server, outside the auth guard.
pub(crate) fn router(handler: Arc<DefaultRequestHandler>) -> Router {
    Router::new().route("/", post(rpc)).with_state(handler)
}

/// The v0.3 agent card for a v1.0 one, served at `url`.
pub(crate) fn card(card: &AgentCard, url: &str, bearer: bool) -> Value {
    let skills: Vec<Value> = card
        .skills
        .iter()
        .map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "tags": skill.tags,
                "examples": skill.examples,
            })
        })
        .collect();
    let v03 = json!({
        "name": card.name,
        "description": card.description,
        "url": url,
        "version": card.version,
        "protocolVersion": "0.3.0",
        "preferredTransport": "JSONRPC",
        "capabilities": {
            "streaming": card.capabilities.streaming.unwrap_or(false),
            "pushNotifications": false,
        },
        "defaultInputModes": card.default_input_modes,
        "defaultOutputModes": card.default_output_modes,
        "skills": skills,
    });
    with_present(
        v03,
        [
            (
                "securitySchemes",
                bearer.then(|| json!({"bearer": {"type": "http", "scheme": "bearer"}})),
            ),
            ("security", bearer.then(|| json!([{"bearer": []}]))),
        ],
    )
}

async fn rpc(
    State(handler): State<Arc<DefaultRequestHandler>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let params = service_params(&headers);
    let body = request.get("params").unwrap_or(&Value::Null);
    let method = request.get("method").and_then(Value::as_str);
    let result = match method.unwrap_or_default() {
        "message/send" => match send_request(body) {
            Ok(req) => handler
                .send_message(&params, req)
                .await
                .map(|response| match response {
                    SendMessageResponse::Task(task) => task_json(&task),
                    SendMessageResponse::Message(message) => message_json(&message),
                }),
            Err(error) => Err(error),
        },
        "message/stream" => {
            let stream = match send_request(body) {
                Ok(req) => handler.send_streaming_message(&params, req).await,
                Err(error) => Err(error),
            };
            return match stream {
                Ok(events) => Sse::new(events.map(move |event| {
                    let frame = match event {
                        Ok(event) => ok(&id, event_json(&event)),
                        Err(error) => err(&id, error.code, &error.message),
                    };
                    Ok::<_, std::convert::Infallible>(Event::default().data(frame.to_string()))
                }))
                .into_response(),
                Err(error) => Json(err(&id, error.code, &error.message)).into_response(),
            };
        }
        "tasks/get" => match task_id(body) {
            Ok(id) => handler
                .get_task(
                    &params,
                    GetTaskRequest {
                        id,
                        history_length: body["historyLength"].as_i64().map(|n| n as i32),
                        tenant: None,
                    },
                )
                .await
                .map(|task| task_json(&task)),
            Err(error) => Err(error),
        },
        "tasks/cancel" => match task_id(body) {
            Ok(id) => handler
                .cancel_task(
                    &params,
                    CancelTaskRequest {
                        id,
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .map(|task| task_json(&task)),
            Err(error) => Err(error),
        },
        other => Err(A2AError::new(
            METHOD_NOT_FOUND,
            format!("method `{other}` not found"),
        )),
    };
    Json(match result {
        Ok(result) => ok(&id, result),
        Err(error) => err(&id, error.code, &error.message),
    })
    .into_response()
}

/// Request headers as the v1.0 handler reads them: lowercase names.
fn service_params(headers: &HeaderMap) -> ServiceParams {
    let mut params = ServiceParams::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            params
                .entry(name.as_str().to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    params
}

fn send_request(params: &Value) -> Result<SendMessageRequest, A2AError> {
    let message = message_from_json(&params["message"])?;
    // v0.3 waits for the task to finish unless told `blocking: false`.
    let return_immediately = params
        .pointer("/configuration/blocking")
        .and_then(Value::as_bool)
        .map(|blocking| !blocking);
    Ok(SendMessageRequest {
        message,
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            return_immediately,
        }),
        metadata: None,
        tenant: None,
    })
}

fn task_id(params: &Value) -> Result<String, A2AError> {
    params["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("`params.id` is required"))
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn invalid(message: impl Into<String>) -> A2AError {
    A2AError::new(INVALID_PARAMS, message)
}

// ------------------------------------------------------- v1.0 -> v0.3 JSON --

fn event_json(event: &StreamResponse) -> Value {
    match event {
        StreamResponse::Task(task) => task_json(task),
        StreamResponse::Message(message) => message_json(message),
        StreamResponse::StatusUpdate(update) => json!({
            "kind": "status-update",
            "taskId": update.task_id,
            "contextId": update.context_id,
            "status": status_json(&update.status),
            "final": update.status.state.is_terminal(),
        }),
        StreamResponse::ArtifactUpdate(update) => json!({
            "kind": "artifact-update",
            "taskId": update.task_id,
            "contextId": update.context_id,
            "artifact": artifact_json(&update.artifact),
            "append": update.append,
            "lastChunk": update.last_chunk,
        }),
    }
}

fn task_json(task: &Task) -> Value {
    let json = json!({
        "kind": "task",
        "id": task.id,
        "contextId": task.context_id,
        "status": status_json(&task.status),
    });
    let artifacts = task
        .artifacts
        .as_ref()
        .map(|artifacts| artifacts.iter().map(artifact_json).collect());
    let history = task
        .history
        .as_ref()
        .map(|history| history.iter().map(message_json).collect());
    with_present(json, [("artifacts", artifacts), ("history", history)])
}

fn status_json(status: &TaskStatus) -> Value {
    with_present(
        json!({"state": state_name(&status.state)}),
        [
            ("message", status.message.as_ref().map(message_json)),
            (
                "timestamp",
                status.timestamp.map(|at| json!(at.to_rfc3339())),
            ),
        ],
    )
}

fn artifact_json(artifact: &Artifact) -> Value {
    json!({
        "artifactId": artifact.artifact_id,
        "name": artifact.name,
        "description": artifact.description,
        "parts": artifact.parts.iter().map(part_json).collect::<Vec<_>>(),
    })
}

fn message_json(message: &Message) -> Value {
    let json = json!({
        "kind": "message",
        "messageId": message.message_id,
        "role": if message.role == Role::User { "user" } else { "agent" },
        "parts": message.parts.iter().map(part_json).collect::<Vec<_>>(),
    });
    with_present(
        json,
        [
            ("contextId", message.context_id.as_ref().map(|id| json!(id))),
            ("taskId", message.task_id.as_ref().map(|id| json!(id))),
        ],
    )
}

fn part_json(part: &Part) -> Value {
    let file = |source: (&str, Value)| {
        let file = with_present(
            json!({ source.0: source.1 }),
            [
                (
                    "mimeType",
                    part.media_type.as_ref().map(|media| json!(media)),
                ),
                ("name", part.filename.as_ref().map(|name| json!(name))),
            ],
        );
        json!({"kind": "file", "file": file})
    };
    match &part.content {
        PartContent::Text(text) => json!({"kind": "text", "text": text}),
        PartContent::Data(data) => json!({"kind": "data", "data": data}),
        PartContent::Raw(bytes) => file(("bytes", json!(BASE64.encode(bytes)))),
        PartContent::Url(url) => file(("uri", json!(url))),
    }
}

/// `object` with each of `fields` that has a value: v0.3 leaves an absent
/// field out rather than sending `null`.
fn with_present<const N: usize>(mut object: Value, fields: [(&str, Option<Value>); N]) -> Value {
    if let Value::Object(map) = &mut object {
        map.extend(
            fields
                .into_iter()
                .filter_map(|(name, value)| Some((name.to_owned(), value?))),
        );
    }
    object
}

pub(crate) fn state_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::InputRequired => "input-required",
        TaskState::Rejected => "rejected",
        TaskState::AuthRequired => "auth-required",
        _ => "unknown",
    }
}

fn state_from_name(name: &str) -> Option<TaskState> {
    Some(match name {
        "submitted" => TaskState::Submitted,
        "working" => TaskState::Working,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed,
        "canceled" => TaskState::Canceled,
        "input-required" => TaskState::InputRequired,
        "rejected" => TaskState::Rejected,
        "auth-required" => TaskState::AuthRequired,
        _ => return None,
    })
}

// ------------------------------------------------------- v0.3 JSON -> v1.0 --

fn message_from_json(json: &Value) -> Result<Message, A2AError> {
    let role = match json["role"].as_str() {
        Some("user") => Role::User,
        Some("agent") => Role::Agent,
        _ => return Err(invalid("`message.role` must be `user` or `agent`")),
    };
    let parts = json["parts"]
        .as_array()
        .ok_or_else(|| invalid("`message.parts` must be an array"))?
        .iter()
        .map(part_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let mut message = Message::new(role, parts);
    if let Some(id) = json["messageId"].as_str() {
        message.message_id = id.to_owned();
    }
    message.context_id = json["contextId"].as_str().map(str::to_owned);
    message.task_id = json["taskId"].as_str().map(str::to_owned);
    Ok(message)
}

fn part_from_json(json: &Value) -> Result<Part, A2AError> {
    let (content, media_type, filename) = match json["kind"].as_str() {
        Some("text") => {
            let text = json["text"]
                .as_str()
                .ok_or_else(|| invalid("a text part needs `text`"))?;
            (PartContent::Text(text.to_owned()), None, None)
        }
        Some("data") => (PartContent::Data(json["data"].clone()), None, None),
        Some("file") => {
            let file = &json["file"];
            let content = if let Some(bytes) = file["bytes"].as_str() {
                PartContent::Raw(
                    BASE64
                        .decode(bytes)
                        .map_err(|e| invalid(format!("file bytes are not base64: {e}")))?,
                )
            } else if let Some(uri) = file["uri"].as_str() {
                PartContent::Url(uri.to_owned())
            } else {
                return Err(invalid("a file part needs `file.bytes` or `file.uri`"));
            };
            let media_type = file["mimeType"].as_str().map(str::to_owned);
            (
                content,
                media_type,
                file["name"].as_str().map(str::to_owned),
            )
        }
        other => return Err(invalid(format!("unknown part kind {other:?}"))),
    };
    let mut part = Part::text("");
    part.content = content;
    part.media_type = media_type;
    part.filename = filename;
    Ok(part)
}

// ---------------------------------------------------------------- calling --

/// Post `message/send` to a v0.3 agent and read its answer as a v1.0 task or message.
pub(crate) async fn send(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    message: &Message,
) -> Result<StreamResponse, crate::RemoteError> {
    let params = json!({"message": message_json(message), "configuration": {"blocking": true}});
    let frame: Value = call(http, url, bearer, "message/send", params)
        .await?
        .json()
        .await?;
    Ok(response_from_frame(&frame)?)
}

/// Post `message/stream` to a v0.3 agent and read its server-sent events as
/// v1.0 events.
pub(crate) async fn stream(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    message: &Message,
) -> Result<BoxStream<'static, Result<StreamResponse, crate::RemoteError>>, crate::RemoteError> {
    let params = json!({"message": message_json(message)});
    let response = call(http, url, bearer, "message/stream", params).await?;
    Ok(async_stream::try_stream! {
        let mut bytes = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = bytes.next().await {
            // JSON escapes carriage returns, so dropping them normalizes CRLF framing.
            buffer.extend(chunk?.iter().filter(|&&byte| byte != b'\r'));
            while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                let block: Vec<u8> = buffer.drain(..end + 2).collect();
                if let Some(data) = sse_data(&block) {
                    let frame: Value = serde_json::from_str(&data)
                        .map_err(|e| A2AError::new(INVALID_PARAMS, format!("bad SSE frame: {e}")))?;
                    yield response_from_frame(&frame)?;
                }
            }
        }
    }
    .boxed())
}

async fn call(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    method: &str,
    params: Value,
) -> Result<reqwest::Response, crate::RemoteError> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut request = http.post(url).json(&body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    #[cfg(feature = "otel")]
    {
        let mut headers = HeaderMap::new();
        rig_otel::inject(&mut headers);
        request = request.headers(headers);
    }
    Ok(request.send().await?.error_for_status()?)
}

/// The `data:` lines of one server-sent event, joined; `None` for an event without data.
fn sse_data(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn response_from_frame(frame: &Value) -> Result<StreamResponse, A2AError> {
    if let Some(error) = frame.get("error") {
        return Err(A2AError::new(
            error["code"].as_i64().unwrap_or(-32603) as i32,
            error["message"].as_str().unwrap_or("unknown error"),
        ));
    }
    response_from_json(&frame["result"])
}

/// A v0.3 result or event as the v1.0 event it means.
fn response_from_json(json: &Value) -> Result<StreamResponse, A2AError> {
    let field = |name: &str| {
        json[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid(format!("`{name}` is required")))
    };
    Ok(match json["kind"].as_str() {
        Some("message") => StreamResponse::Message(message_from_json(json)?),
        Some("task") => StreamResponse::Task(Task {
            id: field("id")?,
            context_id: field("contextId")?,
            status: status_from_json(&json["status"])?,
            artifacts: match json["artifacts"].as_array() {
                Some(artifacts) => Some(
                    artifacts
                        .iter()
                        .map(artifact_from_json)
                        .collect::<Result<_, _>>()?,
                ),
                None => None,
            },
            history: None,
            metadata: None,
        }),
        Some("status-update") => StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: field("taskId")?,
            context_id: field("contextId")?,
            status: status_from_json(&json["status"])?,
            metadata: None,
        }),
        Some("artifact-update") => StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: field("taskId")?,
            context_id: field("contextId")?,
            artifact: artifact_from_json(&json["artifact"])?,
            append: json["append"].as_bool(),
            last_chunk: json["lastChunk"].as_bool(),
            metadata: None,
        }),
        other => return Err(invalid(format!("unknown result kind {other:?}"))),
    })
}

fn status_from_json(json: &Value) -> Result<TaskStatus, A2AError> {
    Ok(TaskStatus {
        state: json["state"]
            .as_str()
            .and_then(state_from_name)
            .unwrap_or(TaskState::Unspecified),
        message: match json.get("message") {
            Some(message) if message.is_object() => Some(message_from_json(message)?),
            _ => None,
        },
        timestamp: json["timestamp"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc)),
    })
}

fn artifact_from_json(json: &Value) -> Result<Artifact, A2AError> {
    Ok(Artifact {
        artifact_id: json["artifactId"].as_str().unwrap_or_default().to_owned(),
        name: json["name"].as_str().map(str::to_owned),
        description: json["description"].as_str().map(str::to_owned),
        parts: json["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(part_from_json)
            .collect::<Result<_, _>>()?,
        metadata: None,
        extensions: None,
    })
}

#[cfg(test)]
mod tests;
