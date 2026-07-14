//! Un-rendered prompt shapes — the values business code carries and hands to
//! the materialize boundary. A draft pairs a [`PromptTemplate`] key with its
//! typed payload; rendering happens only when [`PromptDraft::render`] (or the
//! message/tool variants) is called at the boundary, never while the draft is
//! being assembled or passed around.
//!
//! The discipline: a function that is NOT a materialize boundary returns a
//! draft (or a data struct), never a pre-rendered prompt string. Only
//! `build_completion_request`, the side-call request constructors, the
//! history-insertion sites, and the tool-registry request builders call
//! `render`.

use serde::Serialize;

use crate::language_model::{
    LanguageModelRequestMessage, LanguageModelRequestTool, MessageContent, Role,
};
use crate::prompt::renderer::render;
use crate::prompt::template::PromptTemplate;

/// A template key + its typed payload, awaiting a single render at the
/// boundary.
pub struct PromptDraft<D> {
    pub template: PromptTemplate,
    pub data: D,
}

impl<D: Serialize> PromptDraft<D> {
    /// Create a draft. Convenience so callers read `PromptDraft::new(key, data)`.
    pub const fn new(template: PromptTemplate, data: D) -> Self {
        Self { template, data }
    }

    /// Materialize the draft into its final prompt string. The boundary call.
    pub fn render(self) -> anyhow::Result<String> {
        render(self.template, &self.data)
    }
}

/// A draft for a single request message (role + template + payload + cache
/// flag). Rendered into a `LanguageModelRequestMessage` at the boundary.
pub struct PromptMessageDraft<D> {
    pub role: Role,
    pub template: PromptTemplate,
    pub data: D,
    pub cache: bool,
}

impl<D: Serialize> PromptMessageDraft<D> {
    pub const fn new(role: Role, template: PromptTemplate, data: D, cache: bool) -> Self {
        Self {
            role,
            template,
            data,
            cache,
        }
    }

    /// Materialize the message. The boundary call for history insertion.
    pub fn render(self) -> anyhow::Result<LanguageModelRequestMessage> {
        Ok(LanguageModelRequestMessage {
            role: self.role,
            content: vec![MessageContent::Text(render(self.template, &self.data)?)],
            cache: self.cache,
        })
    }
}

/// A draft for a tool definition advertised to the model: name, a description
/// rendered from a template, the JSON schema, and the streaming flag.
pub struct PromptToolDraft<D> {
    pub name: String,
    pub template: PromptTemplate,
    pub data: D,
    pub input_schema: serde_json::Value,
    pub use_input_streaming: bool,
}

impl<D: Serialize> PromptToolDraft<D> {
    pub fn new(
        name: impl Into<String>,
        template: PromptTemplate,
        data: D,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            template,
            data,
            input_schema,
            use_input_streaming: false,
        }
    }

    /// Materialize the tool definition. The boundary call for
    /// `to_request_tools*`.
    pub fn render(self) -> anyhow::Result<LanguageModelRequestTool> {
        Ok(LanguageModelRequestTool {
            name: self.name,
            description: render(self.template, &self.data)?,
            input_schema: self.input_schema,
            use_input_streaming: self.use_input_streaming,
        })
    }
}
