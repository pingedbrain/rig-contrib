//! Saving a run's progress in a rig-memory store, so a run that stopped (the
//! process restarted, a step failed) carries on from where it was.

use std::io;
use std::sync::Arc;

use futures::future::BoxFuture;
use rig_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::tool::ToolResult;
use rig_memory::AsyncStore;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::swarm::HANDOFF_TOOL;

/// A rig-memory store's files, behind a trait object.
trait Files: Send + Sync {
    fn read<'a>(&'a self, name: &'a str) -> BoxFuture<'a, io::Result<Vec<u8>>>;
    fn write<'a>(&'a self, name: &'a str, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;
}

impl<S: AsyncStore + Send + Sync + 'static> Files for S {
    fn read<'a>(&'a self, name: &'a str) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        Box::pin(self.read_file_async(name))
    }

    fn write<'a>(&'a self, name: &'a str, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(self.write_file_async(name, bytes))
    }
}

/// Where a swarm or graph keeps its runs' checkpoints: one JSON file per
/// run, `rig-multiagent/<kind>/<run id>.json`, written atomically.
#[derive(Clone)]
pub(crate) struct Checkpoints {
    files: Arc<dyn Files>,
    kind: &'static str,
}

impl Checkpoints {
    pub(crate) fn new(store: impl AsyncStore + Send + Sync + 'static, kind: &'static str) -> Self {
        Self {
            files: Arc::new(store),
            kind,
        }
    }

    fn name(&self, id: &str) -> Result<String, String> {
        let valid = (1..=128).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if !valid {
            return Err(format!(
                "run id {id:?} must be 1-128 letters, digits, '-' or '_'"
            ));
        }
        Ok(format!("rig-multiagent/{}/{id}.json", self.kind))
    }

    /// The run's last checkpoint, if it has one.
    pub(crate) async fn load<T: DeserializeOwned>(&self, id: &str) -> Result<Option<T>, String> {
        let name = self.name(id)?;
        match self.files.read(&name).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("checkpoint {name} is unreadable: {error}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read checkpoint {name}: {error}")),
        }
    }

    /// Replace the run's checkpoint with `state`.
    pub(crate) async fn save<T: Serialize>(&self, id: &str, state: &T) -> Result<(), String> {
        let name = self.name(id)?;
        let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
        self.files
            .write(&name, &bytes)
            .await
            .map_err(|error| format!("could not write checkpoint {name}: {error}"))
    }
}

impl Checkpoints {
    /// The same store, for another kind of checkpoint.
    pub(crate) fn with_kind(&self, kind: &'static str) -> Self {
        Self {
            files: self.files.clone(),
            kind,
        }
    }
}

/// Where one run keeps its progress: a store and a run id. Parts of the run
/// (an agent's turn, a node's run) get their own, from [`Saving::part`].
#[derive(Clone)]
pub(crate) struct Saving {
    pub(crate) checkpoints: Checkpoints,
    pub(crate) run_id: String,
}

impl Saving {
    pub(crate) fn new(checkpoints: Checkpoints, run_id: impl Into<String>) -> Self {
        Self {
            checkpoints,
            run_id: run_id.into(),
        }
    }

    /// This run's id, kept as `kind`: the journal of one agent run, say.
    pub(crate) fn kind(&self, kind: &'static str) -> Self {
        Self::new(self.checkpoints.with_kind(kind), self.run_id.clone())
    }

    /// The `n`th run of the part `name` of this run, kept as `kind`.
    pub(crate) fn part(&self, kind: &'static str, name: &str, n: usize) -> Self {
        Self::new(
            self.checkpoints.with_kind(kind),
            part_id(&self.run_id, name, n),
        )
    }
}

/// A run id for the `n`th run of `name` inside run `parent`, within the
/// 128 characters a run id may have: long ones keep a prefix and a hash.
fn part_id(parent: &str, name: &str, n: usize) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let id = format!("{parent}-{name}-{n}");
    if id.len() <= 128 {
        return id;
    }
    // FNV-1a: stable across processes and versions, unlike the std hasher.
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}-{hash:016x}", &id[..111])
}

/// The results of the tools one agent run executed, kept so that running it
/// again (a resumed turn or node) replays them instead of executing them
/// twice: a call with the same tool and arguments gets the recorded result.
/// Only successful results are kept, so a failed call runs again. The swarm's
/// handoff tool always runs: its effect is on the run itself.
#[derive(Clone)]
pub(crate) struct Journal(Arc<JournalInner>);

struct JournalInner {
    saving: Saving,
    state: std::sync::Mutex<JournalState>,
}

#[derive(Default)]
struct JournalState {
    recorded: Vec<Entry>,
    unreplayed: Vec<Entry>,
    replaying: std::collections::HashMap<EffectId, ToolResult>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    tool: String,
    args: Value,
    result: ToolResult,
}

fn arguments(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_owned()))
}

impl Journal {
    /// The journal kept at `saving`, with what it recorded before.
    pub(crate) async fn open(saving: Saving) -> Result<Self, String> {
        let recorded: Vec<Entry> = saving
            .checkpoints
            .load(&saving.run_id)
            .await?
            .unwrap_or_default();
        Ok(Self(Arc::new(JournalInner {
            saving,
            state: std::sync::Mutex::new(JournalState {
                unreplayed: recorded.clone(),
                recorded,
                replaying: std::collections::HashMap::new(),
            }),
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, JournalState> {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl AgentHook for Journal {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let EffectKind::ToolCall { name, args } = event.kind else {
            return DispatchAction::proceed();
        };
        if name == HANDOFF_TOOL {
            return DispatchAction::proceed();
        }
        let args = arguments(args);
        let mut state = self.state();
        let Some(at) = state
            .unreplayed
            .iter()
            .position(|entry| &entry.tool == name && entry.args == args)
        else {
            return DispatchAction::proceed();
        };
        let entry = state.unreplayed.remove(at);
        state.replaying.insert(event.id, entry.result);
        tracing::info!(tool = %name, "tool call replayed from the journal");
        // The call does not reach the tool; on_outcome answers it.
        DispatchAction::skip("replayed from the run's journal")
    }

    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        let EffectKind::ToolCall { name, args } = event.kind else {
            return OutcomeAction::proceed();
        };
        let replayed = self.state().replaying.remove(&event.id);
        if let Some(result) = replayed {
            return OutcomeAction::replace(Ok(Answered::ToolResult { result }));
        }
        let Some(result) = event.tool_result().filter(|result| result.is_success()) else {
            return OutcomeAction::proceed();
        };
        if name == HANDOFF_TOOL {
            return OutcomeAction::proceed();
        }
        let entries = {
            let mut state = self.state();
            state.recorded.push(Entry {
                tool: name.clone(),
                args: arguments(args),
                result: result.clone(),
            });
            state.recorded.clone()
        };
        let Saving {
            checkpoints,
            run_id,
        } = &self.0.saving;
        if let Err(error) = checkpoints.save(run_id, &entries).await {
            tracing::warn!(%error, "could not record a tool result in the journal");
        }
        OutcomeAction::proceed()
    }
}
