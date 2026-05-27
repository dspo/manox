//! Phase 0.2 spike — 验证 rig-core 0.37 能消费 cx YAML provider 配置。
//!
//! 不复用 cx::lib 内部类型（多数 module-private），而是用最小子集解析
//! `~/.config/cx/cx.providers.config.yaml`，挑出三组 (provider, model, wire_api)：
//!   1. OpenAI Responses API（国产 OpenAI-compatible，例如 百炼/qwen3.6-plus）
//!   2. OpenAI Chat Completions API（同上 OpenAI-compatible 体系，例如 百炼/glm-5）
//!   3. Anthropic Messages API（例如 Packy API/claude-opus-4-7）
//!
//! 由命令行参数控制具体选择，默认值见 `Args::default_*`。
//!
//! 用法：
//!   cargo run --example cx_agent_spike
//!   cargo run --example cx_agent_spike -- \
//!       --responses 百炼:qwen3.6-plus \
//!       --completions 百炼:glm-5 \
//!       --anthropic "Packy API:claude-opus-4-7"

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, GetTokenUsage};
use rig_core::message::Message;
use rig_core::providers::anthropic;
use rig_core::providers::openai;
use rig_core::streaming::{StreamedAssistantContent, ToolCallDeltaContent};
use serde::Deserialize;

const PROMPT: &str = "用一句中文打招呼，并报出你模型 id（不超过 30 字）。";

#[derive(Debug, Deserialize)]
struct CxConfigYaml {
    providers: Vec<ProviderYaml>,
}

#[derive(Debug, Deserialize)]
struct ProviderYaml {
    name: String,
    apikey_source: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, ModelYaml>,
    #[serde(default)]
    endpoints: BTreeMap<String, EndpointYaml>,
}

#[derive(Debug, Deserialize)]
struct ModelYaml {
    #[serde(default)]
    wire_apis: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EndpointYaml {
    url: String,
}

#[derive(Debug, Clone)]
struct Pick {
    label: String,
    model_id: String,
    base_url: String,
    api_key: String,
}

fn parse_pair(arg: Option<String>, default: &str) -> (String, String) {
    let s = arg.unwrap_or_else(|| default.to_string());
    let mut iter = s.splitn(2, ':');
    let p = iter.next().unwrap_or("").to_string();
    let m = iter.next().unwrap_or("").to_string();
    (p, m)
}

fn parse_args() -> (String, String, String) {
    let mut responses = None::<String>;
    let mut completions = None::<String>;
    let mut anthropic_pair = None::<String>;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--responses" => responses = args.next(),
            "--completions" => completions = args.next(),
            "--anthropic" => anthropic_pair = args.next(),
            "-h" | "--help" => {
                eprintln!(
                    "用法：\n  cargo run --example cx_agent_spike -- \\\n    --responses provider:model \\\n    --completions provider:model \\\n    --anthropic provider:model"
                );
                std::process::exit(0);
            }
            other => eprintln!("未知参数：{other}（忽略）"),
        }
    }
    let r = parse_pair(responses, "百炼:qwen3.6-plus");
    let c = parse_pair(completions, "百炼:glm-5");
    let a = parse_pair(anthropic_pair, "Packy API:claude-opus-4-7");
    (
        format!("{}|{}", r.0, r.1),
        format!("{}|{}", c.0, c.1),
        format!("{}|{}", a.0, a.1),
    )
}

fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/cx.providers.config.yaml"))
}

fn load_config() -> Result<CxConfigYaml> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 cx 配置失败: {}", path.display()))?;
    let cfg: CxConfigYaml = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 yaml 失败: {}", path.display()))?;
    Ok(cfg)
}

fn resolve_apikey(source: &str) -> Result<String> {
    if let Some(rest) = source.strip_prefix("keychain:") {
        if !cfg!(target_os = "macos") {
            bail!("keychain 仅支持 macOS（{rest}）");
        }
        let user = env::var("USER").unwrap_or_default();
        let out = Command::new("security")
            .args(["find-generic-password", "-a", &user, "-s", rest, "-w"])
            .output()
            .with_context(|| format!("调用 security 失败：{rest}"))?;
        if !out.status.success() {
            bail!(
                "Keychain 读取 `{rest}` 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    } else if let Some(rest) = source.strip_prefix("env:") {
        env::var(rest).with_context(|| format!("环境变量 `{rest}` 未设置"))
    } else if let Some(rest) = source.strip_prefix("literal:") {
        Ok(rest.to_string())
    } else {
        bail!("不支持的 apikey_source 格式: `{source}`")
    }
}

fn resolve_pick(cfg: &CxConfigYaml, key: &str, wire_api: &str) -> Result<Pick> {
    let (provider_name, model_id) = key
        .split_once('|')
        .ok_or_else(|| anyhow!("内部 key 解析失败：{key}"))?;
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == provider_name)
        .ok_or_else(|| anyhow!("未找到 provider：{provider_name}"))?;
    let model = provider
        .models
        .get(model_id)
        .ok_or_else(|| anyhow!("provider `{provider_name}` 下找不到 model：{model_id}"))?;
    if !model.wire_apis.iter().any(|w| w == wire_api) && wire_api != "anthropic_url_only" {
        eprintln!("  ⚠ 警告：模型 {model_id} 在 yaml 中未声明 wire_api={wire_api}（仍尝试调用）");
    }
    let endpoint_key = match wire_api {
        "responses" => "responses",
        "completions" => "completions",
        "anthropic" | "anthropic_url_only" => "anthropic",
        _ => bail!("未知 wire_api: {wire_api}"),
    };
    let endpoint = provider
        .endpoints
        .get(endpoint_key)
        .ok_or_else(|| anyhow!("provider `{provider_name}` 没有 endpoint：{endpoint_key}"))?;
    let key_source = provider
        .apikey_source
        .as_deref()
        .ok_or_else(|| anyhow!("provider `{provider_name}` 缺 apikey_source"))?;
    let api_key = resolve_apikey(key_source)?;
    Ok(Pick {
        label: format!("{provider_name}/{model_id}@{wire_api}"),
        model_id: model_id.to_string(),
        base_url: endpoint.url.clone(),
        api_key,
    })
}

fn print_usage<U: GetTokenUsage>(label: &str, response: Option<&U>) {
    match response.and_then(|r| r.token_usage()) {
        Some(u) => println!(
            "  · usage[{label}]: in={} out={} total={} cache_read={} cache_write={} reasoning={}",
            u.input_tokens,
            u.output_tokens,
            u.total_tokens,
            u.cached_input_tokens,
            u.cache_creation_input_tokens,
            u.reasoning_tokens,
        ),
        None => println!("  · usage[{label}]: <无>"),
    }
}

async fn run_responses(pick: Pick) -> Result<()> {
    println!("\n[Responses] {} → {}", pick.label, pick.base_url);
    let client = openai::Client::builder()
        .api_key(pick.api_key.as_str())
        .base_url(pick.base_url.as_str())
        .build()
        .map_err(|e| anyhow!("openai responses client build failed: {e}"))?;
    let model = client.completion_model(pick.model_id.as_str());
    let mut stream = model
        .completion_request(Message::user(PROMPT))
        .max_tokens(256)
        .stream()
        .await
        .map_err(|e| anyhow!("responses stream() 失败: {e}"))?;

    drive_stream("Responses", &mut stream).await?;
    print_usage("Responses", stream.response.as_ref());
    Ok(())
}

async fn run_completions(pick: Pick) -> Result<()> {
    println!("\n[Completions] {} → {}", pick.label, pick.base_url);
    let client = openai::CompletionsClient::builder()
        .api_key(pick.api_key.as_str())
        .base_url(pick.base_url.as_str())
        .build()
        .map_err(|e| anyhow!("openai completions client build failed: {e}"))?;
    let model = client.completion_model(pick.model_id.as_str());
    let mut stream = model
        .completion_request(Message::user(PROMPT))
        .max_tokens(256)
        .stream()
        .await
        .map_err(|e| anyhow!("completions stream() 失败: {e}"))?;

    drive_stream("Completions", &mut stream).await?;
    print_usage("Completions", stream.response.as_ref());
    Ok(())
}

async fn run_anthropic(pick: Pick) -> Result<()> {
    println!("\n[Anthropic] {} → {}", pick.label, pick.base_url);
    let client = anthropic::Client::builder()
        .api_key(pick.api_key.as_str())
        .base_url(pick.base_url.as_str())
        .build()
        .map_err(|e| anyhow!("anthropic client build failed: {e}"))?;
    let model = client.completion_model(pick.model_id.as_str());
    let mut stream = model
        .completion_request(Message::user(PROMPT))
        .max_tokens(256)
        .stream()
        .await
        .map_err(|e| anyhow!("anthropic stream() 失败: {e}"))?;

    drive_stream("Anthropic", &mut stream).await?;
    print_usage("Anthropic", stream.response.as_ref());
    Ok(())
}

async fn drive_stream<R>(
    label: &str,
    stream: &mut rig_core::streaming::StreamingCompletionResponse<R>,
) -> Result<()>
where
    R: Clone + Unpin + GetTokenUsage,
{
    print!("  ");
    let mut text_chunks = 0usize;
    let mut tool_call_deltas = 0usize;
    let mut reasoning_chunks = 0usize;
    while let Some(chunk) = stream.next().await {
        match chunk.map_err(|e| anyhow!("[{label}] 流出错: {e}"))? {
            StreamedAssistantContent::Text(t) => {
                text_chunks += 1;
                use std::io::Write as _;
                print!("{}", t.text);
                std::io::stdout().flush().ok();
            }
            StreamedAssistantContent::ToolCallDelta { content, .. } => {
                tool_call_deltas += 1;
                if let ToolCallDeltaContent::Name(n) = content {
                    eprint!("[tool_call:{n}]");
                }
            }
            StreamedAssistantContent::ToolCall { tool_call, .. } => {
                eprint!("[tool_call:{}]", tool_call.function.name);
            }
            StreamedAssistantContent::Reasoning(_)
            | StreamedAssistantContent::ReasoningDelta { .. } => {
                reasoning_chunks += 1;
            }
            StreamedAssistantContent::Final(_) => {}
        }
    }
    println!();
    println!(
        "  · stats[{label}]: text_chunks={text_chunks}, tool_deltas={tool_call_deltas}, reasoning={reasoning_chunks}"
    );
    Ok(())
}

fn main() -> Result<()> {
    let (responses_key, completions_key, anthropic_key) = parse_args();
    let cfg = load_config()?;

    let r_pick = resolve_pick(&cfg, &responses_key, "responses")?;
    let c_pick = resolve_pick(&cfg, &completions_key, "completions")?;
    let a_pick = resolve_pick(&cfg, &anthropic_key, "anthropic")?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        // 串行跑，方便看输出。每个 wire API 单独控制超时。
        let _ = tokio::time::timeout(Duration::from_secs(60), run_responses(r_pick))
            .await
            .map_err(|_| anyhow!("Responses 超时"))?
            .map_err(|e| eprintln!("[Responses] 失败：{e:?}"));
        let _ = tokio::time::timeout(Duration::from_secs(60), run_completions(c_pick))
            .await
            .map_err(|_| anyhow!("Completions 超时"))?
            .map_err(|e| eprintln!("[Completions] 失败：{e:?}"));
        let _ = tokio::time::timeout(Duration::from_secs(60), run_anthropic(a_pick))
            .await
            .map_err(|_| anyhow!("Anthropic 超时"))?
            .map_err(|e| eprintln!("[Anthropic] 失败：{e:?}"));
        Ok::<(), anyhow::Error>(())
    })?;

    println!("\n✓ spike 完成");
    Ok(())
}
