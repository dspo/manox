//! ProviderAdapterConfig → 具体 rig client 的工厂。
//!
//! Phase 1.2 先实现 Anthropic（最简单）；Phase 1.4 扩到三种 wire API。

use anyhow::Result;

use crate::cx_agent::config::ProviderAdapterConfig;
use crate::cx_agent::provider::ProviderAdapter;
use crate::cx_agent::provider::rig::RigAdapter;

pub fn build(config: ProviderAdapterConfig) -> Result<Box<dyn ProviderAdapter>> {
    Ok(Box::new(RigAdapter::new(config)?))
}
