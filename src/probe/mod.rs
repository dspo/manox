use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::{
    CopilotAuth, CxConfig, ProviderConfig, WireApi, build_all_models, cx_state_dir, resolve_apikey,
};

use crate::probe::types::{ProbeCellResult, ProbeRow, ProbeStatus};

pub mod db;
pub mod tui;
pub mod types;
pub mod view;

pub(crate) fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().unwrap())
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("创建 HTTP client 失败")
    })
}

/// 运行自动探测（不启动 TUI）
pub fn run_probe_auto(config: &CxConfig, provider_filter: Option<String>) -> Result<()> {
    let db_path = cx_state_dir()?.join("cx.db");
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("打开数据库失败: {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    db::init_probe_schema(&conn)?;

    let rows = build_probe_rows(config, &conn, provider_filter)?;

    println!("开始自动探测...");
    let mut total = 0;
    let mut success = 0;
    let mut failed = 0;

    for row in &rows {
        for wire_api in [WireApi::Anthropic, WireApi::Responses, WireApi::Completions] {
            if let Some(result) = row.results.get(&wire_api) {
                if result.status == ProbeStatus::NotApplicable {
                    continue;
                }

                if let Some(provider) = config
                    .providers
                    .iter()
                    .find(|p| p.name == row.provider_name)
                {
                    let endpoint = provider
                        .normalized_endpoints()
                        .into_iter()
                        .find(|e| WireApi::from_str(&e.wire_api) == wire_api);

                    if let Some(endpoint) = endpoint {
                        total += 1;
                        let auth = CopilotAuth::from_endpoint(&endpoint);
                        let configured = row
                            .results
                            .get(&wire_api)
                            .map(|r| r.configured)
                            .unwrap_or(true);
                        let result =
                            do_probe(provider, &endpoint.url, wire_api, &row.model_id, auth);

                        match result {
                            Ok(mut result) => {
                                result.configured = configured;
                                db::save_probe_result(
                                    &conn,
                                    &row.provider_name,
                                    &row.model_id,
                                    wire_api,
                                    &result,
                                )?;
                                if result.status == ProbeStatus::Available {
                                    success += 1;
                                    println!(
                                        "✓ {} {} {} - {}ms",
                                        row.provider_name,
                                        row.model_id,
                                        wire_api.display(),
                                        result.latency_ms.unwrap_or(0)
                                    );
                                } else {
                                    failed += 1;
                                    println!(
                                        "✗ {} {} {} - {:?}",
                                        row.provider_name,
                                        row.model_id,
                                        wire_api.display(),
                                        result.status
                                    );
                                }
                            }
                            Err(e) => {
                                failed += 1;
                                println!(
                                    "✗ {} {} {} - 错误: {}",
                                    row.provider_name,
                                    row.model_id,
                                    wire_api.display(),
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    println!(
        "\n探测完成: 总计={}, 成功={}, 失败={}",
        total, success, failed
    );
    Ok(())
}

/// 运行 Probe TUI
pub fn run_probe_tui(config: &CxConfig, provider_filter: Option<String>) -> Result<()> {
    let db_path = cx_state_dir()?.join("cx.db");
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("打开数据库失败: {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    db::init_probe_schema(&conn)?;

    let rows = build_probe_rows(config, &conn, provider_filter)?;

    tui::run_tui(rows, config, &conn)
}

pub(crate) fn build_probe_rows(
    config: &CxConfig,
    conn: &rusqlite::Connection,
    provider_filter: Option<String>,
) -> Result<Vec<ProbeRow>> {
    let all_models = build_all_models(config);
    let probe_results = db::load_probe_results(conn)?;

    // 解析 provider 筛选条件
    let filter_providers: Option<Vec<String>> = provider_filter.as_ref().map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });

    let mut model_set = std::collections::BTreeSet::new();
    for model in &all_models {
        model_set.insert((model.provider_name.clone(), model.id.clone()));
    }

    let mut rows = Vec::new();
    for (provider_name, model_id) in model_set {
        // 如果指定了 provider 筛选，则只显示匹配的 provider
        if let Some(ref filters) = filter_providers {
            if !filters.iter().any(|f| f == &provider_name) {
                continue;
            }
        }

        let mut results = HashMap::new();

        for wire_api in [WireApi::Anthropic, WireApi::Responses, WireApi::Completions] {
            let key = probe_result_key(&provider_name, &model_id, wire_api);

            // 检查用户是否配置了该 wire_api
            let configured = all_models.iter().any(|m| {
                m.provider_name == provider_name && m.id == model_id && m.wire_api == wire_api
            });

            if let Some(result) = probe_results.get(&key) {
                let mut result = result.clone();
                result.configured = configured;
                results.insert(wire_api, result);
            } else {
                // 未探测过，但始终显示（不隐藏未配置的）
                results.insert(
                    wire_api,
                    ProbeCellResult {
                        status: ProbeStatus::Unknown,
                        latency_ms: None,
                        http_status: None,
                        error_message: None,
                        configured,
                    },
                );
            }
        }

        rows.push(ProbeRow {
            provider_name,
            model_id,
            results,
        });
    }

    Ok(rows)
}

pub(crate) fn probe_result_key(provider_name: &str, model_id: &str, wire_api: WireApi) -> String {
    format!("{}\0{}\0{}", provider_name, wire_api.display(), model_id)
}

/// 解析 model_id，处理 [1m], [3m] 等后缀
fn resolve_api_model_id(model_id: &str) -> Cow<'_, str> {
    use regex::Regex;
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\d+[m]\]$").unwrap());
    if re.is_match(model_id) {
        Cow::Owned(re.replace(model_id, "").to_string())
    } else {
        Cow::Borrowed(model_id)
    }
}

pub fn do_probe(
    provider: &ProviderConfig,
    endpoint_url: &str,
    wire_api: WireApi,
    model_id: &str,
    auth: CopilotAuth,
) -> Result<ProbeCellResult> {
    let api_key = match &provider.apikey_source {
        Some(source) => match resolve_apikey(source) {
            Ok(key) => key,
            Err(e) => {
                return Ok(ProbeCellResult {
                    status: ProbeStatus::ClientError,
                    latency_ms: None,
                    http_status: None,
                    error_message: Some(format!("获取 API Key 失败: {e}")),
                    configured: true,
                });
            }
        },
        None => {
            return Ok(ProbeCellResult {
                status: ProbeStatus::ClientError,
                latency_ms: None,
                http_status: None,
                error_message: Some("未配置 API Key".to_string()),
                configured: true,
            });
        }
    };

    // 解析 model_id，处理 [1m] 等后缀
    let resolved_model_id = resolve_api_model_id(model_id);

    // 根据 wire_api 确定完整 URL（兼容基础 URL 和完整 URL）
    let url = match wire_api {
        WireApi::Anthropic => {
            if endpoint_url.ends_with("/v1/messages") {
                endpoint_url.to_string()
            } else {
                format!("{}/v1/messages", endpoint_url.trim_end_matches('/'))
            }
        }
        WireApi::Completions => {
            if endpoint_url.ends_with("/chat/completions") {
                endpoint_url.to_string()
            } else {
                format!("{}/chat/completions", endpoint_url.trim_end_matches('/'))
            }
        }
        WireApi::Responses => {
            if endpoint_url.ends_with("/responses") {
                endpoint_url.to_string()
            } else {
                format!("{}/responses", endpoint_url.trim_end_matches('/'))
            }
        }
        WireApi::Unavailable => endpoint_url.to_string(),
    };

    match wire_api {
        WireApi::Anthropic | WireApi::Completions | WireApi::Responses => {
            let body = match wire_api {
                WireApi::Anthropic => json!({
                    "model": resolved_model_id,
                    "max_tokens": 5,
                    "messages": [{"role": "user", "content": "hi"}]
                }),
                WireApi::Completions => json!({
                    "model": resolved_model_id,
                    "max_tokens": 5,
                    "messages": [{"role": "user", "content": "hi"}]
                }),
                WireApi::Responses => json!({
                    "model": resolved_model_id,
                    "max_output_tokens": 5,
                    "input": [{"role": "user", "content": "hi"}]
                }),
                _ => unreachable!(),
            };
            probe_endpoint(&url, &api_key, wire_api, auth, body)
        }
        WireApi::Unavailable => Ok(ProbeCellResult {
            status: ProbeStatus::NotApplicable,
            latency_ms: None,
            http_status: None,
            error_message: None,
            configured: true,
        }),
    }
}

fn probe_endpoint(
    url: &str,
    api_key: &str,
    wire_api: WireApi,
    auth: CopilotAuth,
    body: serde_json::Value,
) -> Result<ProbeCellResult> {
    let start = Instant::now();

    runtime().block_on(async {
        let mut request = http_client()
            .post(url)
            .header("Content-Type", "application/json")
            .json(&body);

        // 认证头由协议决定：
        // - OpenAI 协议（responses/completions）统一使用 Authorization: Bearer
        // - Anthropic 协议默认使用 x-api-key，并补充 anthropic-version 头；
        //   若端点显式配置 copilot_auth: bearer_token，则改用 Bearer
        request = match wire_api {
            WireApi::Responses | WireApi::Completions => request.bearer_auth(api_key),
            WireApi::Anthropic => {
                let request = request.header("anthropic-version", "2023-06-01");
                match auth {
                    CopilotAuth::BearerToken => request.bearer_auth(api_key),
                    CopilotAuth::ApiKey => request.header("x-api-key", api_key),
                }
            }
            WireApi::Unavailable => match auth {
                CopilotAuth::BearerToken => request.bearer_auth(api_key),
                CopilotAuth::ApiKey => request.header("x-api-key", api_key),
            },
        };

        let response = request.send().await.context("调用 API 失败")?;

        let status = response.status();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            let error_body = response.text().await.ok();
            return Ok(ProbeCellResult {
                status: if status.is_server_error() {
                    ProbeStatus::ServerError
                } else {
                    ProbeStatus::ClientError
                },
                latency_ms: None,
                http_status: Some(status.as_u16()),
                error_message: error_body,
                configured: true,
            });
        }

        Ok(ProbeCellResult {
            status: ProbeStatus::Available,
            latency_ms: Some(latency_ms),
            http_status: Some(status.as_u16()),
            error_message: None,
            configured: true,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_api_model_id_with_suffix() {
        assert_eq!(resolve_api_model_id("gpt-4o[1m]"), "gpt-4o");
        assert_eq!(resolve_api_model_id("claude-3-opus[999m]"), "claude-3-opus");
        assert_eq!(resolve_api_model_id("model[123m]"), "model");
    }

    #[test]
    fn test_resolve_api_model_id_without_suffix() {
        assert_eq!(resolve_api_model_id("gpt-4o"), "gpt-4o");
        assert_eq!(resolve_api_model_id("claude-3-opus"), "claude-3-opus");
        assert_eq!(resolve_api_model_id("model"), "model");
    }

    #[test]
    fn test_resolve_api_model_id_edge_cases() {
        // 空字符串
        assert_eq!(resolve_api_model_id(""), "");
        // 只有后缀
        assert_eq!(resolve_api_model_id("[1m]"), "");
        // 类似但不匹配的模式
        assert_eq!(resolve_api_model_id("model[1]"), "model[1]");
        assert_eq!(resolve_api_model_id("model[1mm]"), "model[1mm]");
        assert_eq!(resolve_api_model_id("model[m]"), "model[m]");
    }

    #[test]
    fn test_probe_result_key() {
        use crate::WireApi;
        let key = probe_result_key("openai", "gpt-4o", WireApi::Completions);
        assert_eq!(key, "openai\0completions\0gpt-4o");

        let key = probe_result_key("anthropic", "claude-3", WireApi::Anthropic);
        assert_eq!(key, "anthropic\0anthropic\0claude-3");
    }
}
