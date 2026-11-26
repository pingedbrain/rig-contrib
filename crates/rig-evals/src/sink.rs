//! Where reports go.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use rig_core::error::BoxError;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};

use crate::Report;

/// Receives an experiment's report when it finishes.
pub trait Sink: WasmCompatSend + WasmCompatSync {
    /// Record `report`.
    fn record<'a>(&'a self, report: &'a Report) -> WasmBoxedFuture<'a, Result<(), SinkError>>;
}

/// Why a sink could not record a report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// A file could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// Standard output could not be written.
    #[error("cannot write to stdout: {0}")]
    Stdout(std::io::Error),
    /// The report could not be serialized.
    #[error("cannot serialize the report: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Any other failure, such as a remote service's.
    #[error(transparent)]
    Other(BoxError),
}

/// Write `contents` to `path`, creating its directory.
fn write(path: &Path, contents: &str) -> Result<(), SinkError> {
    let written = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => std::fs::create_dir_all(dir),
        _ => Ok(()),
    }
    .and_then(|()| std::fs::write(path, contents));
    written.map_err(|source| SinkError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Writes the report as JSON ([`Report::to_json`]) to a file.
#[derive(Debug, Clone)]
pub struct JsonFile(PathBuf);

impl JsonFile {
    /// Write to `path`, creating its directory.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

impl Sink for JsonFile {
    fn record<'a>(&'a self, report: &'a Report) -> WasmBoxedFuture<'a, Result<(), SinkError>> {
        Box::pin(async move { write(&self.0, &report.to_json()?) })
    }
}

/// Writes the report as JUnit XML ([`Report::to_junit`]) to a file, for CI
/// test reports.
#[derive(Debug, Clone)]
pub struct JunitFile(PathBuf);

impl JunitFile {
    /// Write to `path`, creating its directory.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

impl Sink for JunitFile {
    fn record<'a>(&'a self, report: &'a Report) -> WasmBoxedFuture<'a, Result<(), SinkError>> {
        Box::pin(async move { write(&self.0, &report.to_junit()) })
    }
}

/// Writes the report as Markdown ([`Report::to_markdown`]) to a file (such as
/// `$GITHUB_STEP_SUMMARY`) or to standard output.
#[derive(Debug, Clone)]
pub struct Markdown(Option<PathBuf>);

impl Markdown {
    /// Write to `path`, creating its directory.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self(Some(path.into()))
    }

    /// Print to standard output.
    pub fn stdout() -> Self {
        Self(None)
    }
}

impl Sink for Markdown {
    fn record<'a>(&'a self, report: &'a Report) -> WasmBoxedFuture<'a, Result<(), SinkError>> {
        Box::pin(async move {
            let markdown = report.to_markdown();
            match &self.0 {
                Some(path) => write(path, &markdown),
                None => std::io::stdout()
                    .lock()
                    .write_all(markdown.as_bytes())
                    .map_err(SinkError::Stdout),
            }
        })
    }
}
