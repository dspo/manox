//! Provider-neutral 消息 IR — 见 `docs/cx-agent-plan.md` §4.1。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum CxMessage {
    System {
        content: String,
    },
    User {
        content: Vec<CxContent>,
    },
    Assistant {
        content: Vec<CxContent>,
    },
    ToolResult {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CxContent {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        arguments: serde_json::Value,
    },
}

impl CxContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

impl CxMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![CxContent::text(text)],
        }
    }
}
