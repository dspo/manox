//! `apikey_source` resolution.
//!
//! Supports four sources:
//! - `keychain:SERVICE` — macOS Keychain (`security find-generic-password -a $USER -s SERVICE -w`)
//! - `env:VAR` — environment variable
//! - `literal:...` — literal value
//! - `$(shell ...)` — shell command output
//!
//! No keychain crate is pulled in (zero extra dependencies, matching cx).

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Resolve an `apikey_source` into the actual API key string.
pub fn resolve_apikey(source: &str) -> Result<String> {
    if let Some(rest) = source.strip_prefix("keychain:") {
        keychain_secret(rest)
    } else if let Some(rest) = source.strip_prefix("env:") {
        std::env::var(rest).with_context(|| format!("环境变量 `{rest}` 未设置"))
    } else if let Some(rest) = source.strip_prefix("literal:") {
        Ok(rest.to_string())
    } else if source.starts_with("$(") {
        let cmd = source.trim_start_matches("$(").trim_end_matches(')');
        let output = Command::new("sh")
            .args(["-c", cmd])
            .output()
            .with_context(|| format!("执行 shell 命令失败: {cmd}"))?;
        if !output.status.success() {
            bail!("shell 命令 `{cmd}` 执行失败");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    } else {
        bail!("不支持的 apikey_source 格式: `{source}`")
    }
}

/// Read a generic password from the macOS Keychain.
fn keychain_secret(service: &str) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!(
            "`keychain:` 仅支持 macOS Keychain，请改用 `env:` 配置 `{service}`。"
        );
    }

    let user = std::env::var("USER").unwrap_or_default();
    let output = Command::new("security")
        .args(["find-generic-password", "-a", &user, "-s", service, "-w"])
        .output()
        .with_context(|| format!("调用 macOS Keychain 失败，service={service}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "无法从 Keychain 读取 `{service}`: {}",
            if stderr.is_empty() {
                "未知错误".to_string()
            } else {
                stderr
            }
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_literal() {
        assert_eq!(resolve_apikey("literal:sk-abc").unwrap(), "sk-abc");
    }

    #[test]
    fn resolve_env() {
        // HOME is set on every platform.
        let v = resolve_apikey("env:HOME").expect("env:HOME 应成功");
        assert!(!v.is_empty());
    }

    #[test]
    fn resolve_shell() {
        let v = resolve_apikey("$(echo hello)").unwrap();
        assert_eq!(v.trim(), "hello");
    }

    #[test]
    fn resolve_unsupported() {
        assert!(resolve_apikey("foo:bar").is_err());
    }
}
