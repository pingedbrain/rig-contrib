//! Saving a run's progress in a rig-memory store, so a run that stopped (the
//! process restarted, a step failed) carries on from where it was.

use std::io;
use std::sync::Arc;

use futures::future::BoxFuture;
use rig_memory::AsyncStore;
use serde::Serialize;
use serde::de::DeserializeOwned;

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
