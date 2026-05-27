//! Stream events — provider-neutral 流式输出 IR。见 §4.4。

#[derive(Debug, Clone)]
pub enum CxStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        partial: String,
    },
    ToolCallDone {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    },
    Done,
    Error(String),
}
