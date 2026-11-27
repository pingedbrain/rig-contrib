//! Cases and datasets: what an experiment runs, and what each run is checked
//! against.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One input to run the target on, with the references evaluators check the
/// run against: the expected answer, the expected tool calls, and
/// assertions in plain language (for an LLM judge).
///
/// A case is single-turn (`input`) or multi-turn (`turns`): see
/// [`Case::turns`]. The same three references as AWS Bedrock AgentCore's
/// evaluation reference inputs and Strands Evals' cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "CaseWire")]
#[non_exhaustive]
pub struct Case {
    /// Names the case in reports; unique within a dataset.
    pub id: String,
    /// The prompt the target gets. Empty for a multi-turn case, which sets
    /// `turns` instead; a case sets one or the other.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub input: String,
    /// The user turns of a multi-turn case, sent in order on one
    /// conversation. Empty for a single-turn case.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<String>,
    /// The expected answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The tool calls the target is expected to make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory: Option<Vec<ExpectedCall>>,
    /// Statements the answer must satisfy, in plain language.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<String>,
    /// Anything else, kept in reports.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

impl Case {
    /// A case `id` with prompt `input` and no references.
    pub fn new(id: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            input: input.into(),
            turns: Vec::new(),
            expected: None,
            trajectory: None,
            assertions: Vec::new(),
            metadata: Map::new(),
        }
    }

    /// A multi-turn case `id`: each of `turns` is a user message, sent in
    /// order on one conversation, so a turn can refer to the answers before
    /// it. The target answers the last turn; evaluators see the whole
    /// transcript as well (see [`Trial::transcript`](crate::Trial::transcript)).
    ///
    /// ```
    /// use rig_evals::Case;
    /// let case = Case::multi_turn("trip", ["Plan a trip to Lisbon.", "Which month is best?"]);
    /// assert_eq!(case.prompts().len(), 2);
    /// ```
    pub fn multi_turn<I, S>(id: impl Into<String>, turns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(id, "").turns(turns)
    }

    /// Make this a multi-turn case with `turns`, replacing `input`.
    #[must_use]
    pub fn turns<I, S>(mut self, turns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.input.clear();
        self.turns = turns.into_iter().map(Into::into).collect();
        self
    }

    /// The user turns, in order: `turns` of a multi-turn case, else the one
    /// `input`.
    pub fn prompts(&self) -> Vec<&str> {
        if self.turns.is_empty() {
            vec![self.input.as_str()]
        } else {
            self.turns.iter().map(String::as_str).collect()
        }
    }

    /// Set the expected answer.
    #[must_use]
    pub fn expected(mut self, expected: impl Into<String>) -> Self {
        self.expected = Some(expected.into());
        self
    }

    /// Set the expected tool calls.
    #[must_use]
    pub fn trajectory(mut self, calls: impl IntoIterator<Item = ExpectedCall>) -> Self {
        self.trajectory = Some(calls.into_iter().collect());
        self
    }

    /// Add an assertion.
    #[must_use]
    pub fn assertion(mut self, assertion: impl Into<String>) -> Self {
        self.assertions.push(assertion.into());
        self
    }

    /// Add a metadata entry.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// The JSON form of a [`Case`]: a case has `input` or `turns`, not both.
#[derive(Deserialize)]
struct CaseWire {
    id: String,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    turns: Vec<String>,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    trajectory: Option<Vec<ExpectedCall>>,
    #[serde(default)]
    assertions: Vec<String>,
    #[serde(default)]
    metadata: Map<String, Value>,
}

impl TryFrom<CaseWire> for Case {
    type Error = String;

    fn try_from(wire: CaseWire) -> Result<Self, Self::Error> {
        let input = match (wire.input, wire.turns.is_empty()) {
            (Some(input), true) => input,
            (None, false) => String::new(),
            (Some(_), false) => {
                return Err(format!("case {:?} sets both input and turns", wire.id));
            }
            (None, true) => return Err(format!("case {:?} sets neither input nor turns", wire.id)),
        };
        Ok(Self {
            id: wire.id,
            input,
            turns: wire.turns,
            expected: wire.expected,
            trajectory: wire.trajectory,
            assertions: wire.assertions,
            metadata: wire.metadata,
        })
    }
}

/// A tool call a case expects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedCall {
    /// The tool's name.
    pub name: String,
    /// The arguments; `None` accepts any. An object matches when each of its
    /// keys has the same value in the call (the call may have more keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

impl ExpectedCall {
    /// A call to `name` with any arguments.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: None,
        }
    }

    /// Expect these arguments.
    #[must_use]
    pub fn args(mut self, args: Value) -> Self {
        self.args = Some(args);
        self
    }
}

/// The cases of an experiment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Dataset {
    /// The cases, in order.
    pub cases: Vec<Case>,
}

impl Dataset {
    /// A dataset of `cases`.
    pub fn new(cases: impl IntoIterator<Item = Case>) -> Self {
        Self {
            cases: cases.into_iter().collect(),
        }
    }

    /// Parse a JSON array of cases.
    pub fn from_json(text: &str) -> Result<Self, DatasetError> {
        Ok(serde_json::from_str(text)?)
    }

    /// Parse JSON Lines: one case per line, blank lines skipped.
    pub fn from_jsonl(text: &str) -> Result<Self, DatasetError> {
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|source| DatasetError::Line {
                    line: index + 1,
                    source,
                })
            })
            .collect()
    }

    /// Read a file: `.json` as a JSON array, `.jsonl` or `.ndjson` as JSON
    /// Lines.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DatasetError> {
        let path = path.as_ref();
        let read = || {
            std::fs::read_to_string(path).map_err(|source| DatasetError::Read {
                path: path.to_owned(),
                source,
            })
        };
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("json") => Self::from_json(&read()?),
            Some("jsonl" | "ndjson") => Self::from_jsonl(&read()?),
            _ => Err(DatasetError::UnknownFormat(path.to_owned())),
        }
    }
}

impl From<Vec<Case>> for Dataset {
    fn from(cases: Vec<Case>) -> Self {
        Self { cases }
    }
}

impl FromIterator<Case> for Dataset {
    fn from_iter<I: IntoIterator<Item = Case>>(cases: I) -> Self {
        Self::new(cases)
    }
}

/// Why a dataset could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DatasetError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The JSON is not an array of cases.
    #[error("invalid dataset: {0}")]
    Json(#[from] serde_json::Error),
    /// A JSON Lines line is not a case.
    #[error("invalid case on line {line}: {source}")]
    Line {
        /// The line number, from 1.
        line: usize,
        /// The parse error.
        source: serde_json::Error,
    },
    /// The file's extension is not `.json`, `.jsonl` or `.ndjson`.
    #[error("{0}: expected a .json, .jsonl or .ndjson file")]
    UnknownFormat(PathBuf),
}
