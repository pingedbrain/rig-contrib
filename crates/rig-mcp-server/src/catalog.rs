//! The static resources and prompts a server offers next to its tools.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{Prompt, PromptMessage, Resource, ResourceContents};

/// A prompt's rendering: the arguments a client passed, in, and its messages out.
pub(crate) type Render = Arc<dyn Fn(&HashMap<String, String>) -> Vec<PromptMessage> + Send + Sync>;

/// A resource with its fixed contents, returned by `resources/read`.
pub(crate) struct StaticResource {
    pub(crate) resource: Resource,
    pub(crate) contents: ResourceContents,
}

/// A prompt, listed with its arguments and rendered by `prompts/get`.
pub(crate) struct StaticPrompt {
    pub(crate) prompt: Prompt,
    pub(crate) render: Render,
}
