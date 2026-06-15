use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::{
    build_all_models, cx_state_dir, resolve_apikey, CopilotAuth, CxConfig, ProviderConfig, WireApi,
};

use crate::probe::types::{ProbeCellResult, ProbeRow, ProbeStatus};

pub mod db;
pub mod tui;
pub mod types;
pub mod view;

fn runtime() -> &'static Runtime {
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

                if let Some(provider) = config.providers.iter().find(|p| p.name == row.provider_name) {
                    let endpoint = provider
                        .normalized_endpoints()
                        .into_iter()
                        .find(|e| WireApi::from_str(&e.wire_api) == wire_api);

                    if let Some(endpoint) = endpoint {
                        total += 1;
                        let auth = CopilotAuth::from_endpoint(&endpoint);
                        let configured = row.results.get(&wire_api).map(|r| r.configured).unwrap_or(true);
                        let result = do_probe(provider, &endpoint.url, wire_api, &row.model_id, auth);

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
                                    println!("✓ {} {} {} - {}ms", row.provider_name, row.model_id, wire_api.display(), result.latency_ms.unwrap_or(0));
                                } else {
                                    failed += 1;
                                    println!("✗ {} {} {} - {:?}", row.provider_name, row.model_id, wire_api.display(), result.status);
                                }
                            }
                            Err(e) => {
                                failed += 1;
                                println!("✗ {} {} {} - 错误: {}", row.provider_name, row.model_id, wire_api.display(), e);
                            }
                        }
                    }
                }
            }
        }
    }

    println!("\n探测完成: 总计={}, 成功={}, 失败={}", total, success, failed);
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
                results.insert(wire_api, ProbeCellResult {
                    status: ProbeStatus::Unknown,
                    latency_ms: None,
                    http_status: None,
                    error_message: None,
                    configured,
                });
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
    format!("{}	{}	{}", provider_name, wire_api.display(), model_id)
}

/// 解析 model_id，处理 [1m], [3m] 等后缀
fn resolve_api_model_id(model_id: &str) -> String {
    use regex::Regex;
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\d+[m]\]$").unwrap());
    if re.is_match(model_id) {
        re.replace(model_id, "").to_string()
    } else {
        model_id.to_string()
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

    match wire_api {
        WireApi::Anthropic => probe_anthropic(&api_key, endpoint_url, &resolved_model_id, auth),
        WireApi::Completions => probe_completions(&api_key, endpoint_url, &resolved_model_id, auth),
        WireApi::Responses => probe_responses(&api_key, endpoint_url, &resolved_model_id, auth),
        WireApi::Unavailable => {
            Ok(ProbeCellResult {
                status: ProbeStatus::NotApplicable,
                latency_ms: None,
                http_status: None,
                error_message: None,
                configured: true,
            })
        }
    }
}

fn probe_anthropic(
    api_key: &str,
    endpoint_url: &str,
    model_id: &str,
    auth: CopilotAuth,
) -> Result<ProbeCellResult> {
    let url = format!("{}/v1/messages", endpoint_url.trim_end_matches('/'));

    let start = Instant::now();

    runtime().block_on(async {
        let mut request = http_client()
            .post(url)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&json!({
                "model": model_id,
                "max_tokens": 5,
                "messages": [{"role": "user", "content": "hi"}]
            }));

        request = match auth {
            CopilotAuth::ApiKey => request.header("x-api-key", api_key),
            CopilotAuth::BearerToken => request.bearer_auth(api_key),
        };

        let response = request
            .send()
            .await
            .context("调用 Anthropic API 失败")?;

        let status = response.status();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            let error_body = response.text().await.ok();
            return Ok(ProbeCellResult {
                status: if status.is_server_error() { ProbeStatus::ServerError } else { ProbeStatus::ClientError },
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

fn probe_completions(
    api_key: &str,
    endpoint_url: &str,
    model_id: &str,
    auth: CopilotAuth,
) -> Result<ProbeCellResult> {
    let url = format!("{}/chat/completions", endpoint_url.trim_end_matches('/'));

    let start = Instant::now();

    runtime().block_on(async {
        let mut request = http_client()
            .post(url)
            .header("Content-Type", "application/json")
            .json(&json!({
                "model": model_id,
                "max_tokens": 5,
                "messages": [{"role": "user", "content": "hi"}]
            }));

        request = match auth {
            CopilotAuth::ApiKey => request.header("x-api-key", api_key),
            CopilotAuth::BearerToken => request.bearer_auth(api_key),
        };

        let response = request
            .send()
            .await
            .context("调用 Completions API 失败")?;

        let status = response.status();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            let error_body = response.text().await.ok();
            return Ok(ProbeCellResult {
                status: if status.is_server_error() { ProbeStatus::ServerError } else { ProbeStatus::ClientError },
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

fn probe_responses(
    api_key: &str,
    endpoint_url: &str,
    model_id: &str,
    auth: CopilotAuth,
) -> Result<ProbeCellResult> {
    let url = format!("{}/responses", endpoint_url.trim_end_matches('/'));

    let start = Instant::now();

    runtime().block_on(async {
        let mut request = http_client()
            .post(url)
            .header("Content-Type", "application/json")
            .json(&json!({
                "model": model_id,
                "max_output_tokens": 5,
                "input": [{"role": "user", "content": "hi"}]
            }));

        request = match auth {
            CopilotAuth::ApiKey => request.header("x-api-key", api_key),
            CopilotAuth::BearerToken => request.bearer_auth(api_key),
        };

        let response = request
            .send()
            .await
            .context("调用 Responses API 失败")?;

        let status = response.status();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            let error_body = response.text().await.ok();
            return Ok(ProbeCellResult {
                status: if status.is_server_error() { ProbeStatus::ServerError } else { ProbeStatus::ClientError },
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
