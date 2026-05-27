//! 单个一次性 tokio current_thread runtime。
//!
//! 由 `run_cx_agent` 在最外层 `block_on`，整个 agent 生命周期（含 streaming、tool exec、TUI）
//! 都在这个 runtime 内跑。保持 cx 现有 reqwest::blocking 调用与本 runtime 互不干扰。

use anyhow::{Context, Result};
use tokio::runtime::{Builder, Runtime};

pub fn build() -> Result<Runtime> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .context("构建 tokio current_thread runtime 失败")
}
