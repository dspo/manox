//! ProviderAdapter trait —— Cx Agent 与具体 wire API 的边界。
//!
//! 唯一接触 `rig_core::*` 的地方在 `rig.rs`；其它代码只能 use `ProviderAdapter` 和
//! `CxStreamEvent`。

pub mod mapping;
pub mod rig;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::cx_agent::events::CxStreamEvent;
use crate::cx_agent::history::CxMessage;

#[derive(Debug, Clone)]
pub struct CxToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CxTurnRequest {
    pub system: Option<String>,
    pub history: Vec<CxMessage>,
    pub tools: Vec<CxToolDefinition>,
    pub model_id: String,
    pub max_tokens: Option<u64>,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream_turn<'a>(
        &'a self,
        request: CxTurnRequest,
    ) -> Result<BoxStream<'a, Result<CxStreamEvent>>>;
}
