//! 文件级解析缓存（基于 mtime + size）。

use anyhow::{Context, Result};
use dirs::home_dir;
use std::fs;
use std::path::{Path, PathBuf};

use super::types::ScanCache;

pub(super) fn cache_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".local/share/cx/stats-cache.json"))
}

pub(super) fn load_cache(path: &Path) -> Result<ScanCache> {
    let bytes = fs::read(path)?;
    let cache: ScanCache = serde_json::from_slice(&bytes)?;
    Ok(cache)
}

pub(super) fn save_cache(path: &Path, cache: &ScanCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(cache)?;
    fs::write(path, json)?;
    Ok(())
}
