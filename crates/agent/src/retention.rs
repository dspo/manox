//! Cache-aware pruning for the model-facing conversation projection.
//!
//! Pruning is deliberately pair-aware: a `ToolUse` and its matching
//! `ToolResult` are either both retained or both removed. Canonical thread
//! history is never mutated.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::language_model::{LanguageModelToolResult, LanguageModelToolUse, MessageContent};
use crate::message::Message;

const PRUNE_COOLDOWN: Duration = Duration::from_secs(90 * 60);
const HOT_PREFIX_BYTES: usize = 160 * 1024;
const MIN_SAVINGS_BYTES: usize = 80 * 1024;

static COOLDOWNS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Hint {
    Keep,
    Useless,
    Supersede(SupersedeKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupersedeKey {
    key: String,
    /// File affected by a Read. Used to enforce the Edit/Write refresh barrier.
    read_path: Option<String>,
}

#[derive(Clone)]
struct ToolPair {
    use_msg: usize,
    use_block: usize,
    result_msg: usize,
    result_block: usize,
    tool_use: LanguageModelToolUse,
    result: LanguageModelToolResult,
}

impl ToolPair {
    fn projected_bytes(&self) -> usize {
        self.result.content.len()
            + self.tool_use.raw_input.len()
            + serde_json::to_string(&self.tool_use.input)
                .map(|s| s.len())
                .unwrap_or_default()
    }
}

fn classify(pair: &ToolPair, cwd: &Path) -> Hint {
    if pair.result.is_error {
        return Hint::Keep;
    }
    if is_useless_poll(&pair.result) {
        return Hint::Useless;
    }
    supersede_key(&pair.tool_use, cwd)
        .map(Hint::Supersede)
        .unwrap_or(Hint::Keep)
}

/// A running BashOutput poll with no incremental body carries no durable state.
/// Its matching ToolUse is removed with it, so provider pairing stays valid.
fn is_useless_poll(result: &LanguageModelToolResult) -> bool {
    if result.tool_name.as_ref() != crate::tools::BASH_OUTPUT {
        return false;
    }
    let Some((header, body)) = result.content.split_once("\n\n") else {
        return false;
    };
    header.starts_with("Shell status: running") && body.trim().is_empty()
}

fn supersede_key(tool_use: &LanguageModelToolUse, cwd: &Path) -> Option<SupersedeKey> {
    let input = &tool_use.input;
    match tool_use.name.as_ref() {
        crate::tools::READ => {
            let raw = input.get("path")?.as_str()?;
            let (path, selector) = crate::tools::path_selector::split_path_and_sel(raw);
            let path = normalize_path(cwd, path);
            let selector = selector
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "Full".to_string());
            Some(SupersedeKey {
                key: format!("Read:{path}:{selector}"),
                read_path: Some(path),
            })
        }
        crate::tools::GREP => Some(SupersedeKey {
            key: format!(
                "Grep:{}:{}:{}:{}:{}",
                json_string(input, "pattern"),
                input_path(input, cwd),
                json_string(input, "glob"),
                json_string(input, "limit"),
                json_string(input, "offset")
            ),
            read_path: None,
        }),
        crate::tools::GLOB => Some(SupersedeKey {
            key: format!(
                "Glob:{}:{}:{}:{}:{}:{}",
                json_string(input, "pattern"),
                input_path(input, cwd),
                json_string(input, "no_ignore"),
                json_string(input, "include_hidden"),
                json_string(input, "include_dirs"),
                json_string(input, "limit")
            ),
            read_path: None,
        }),
        crate::tools::LIST => Some(SupersedeKey {
            key: format!("List:{}", input_path(input, cwd)),
            read_path: None,
        }),
        crate::tools::BASH_OUTPUT => Some(SupersedeKey {
            key: format!("BashOutput:{}", json_string(input, "shell_id")),
            read_path: None,
        }),
        crate::tools::TOOL_SEARCH => Some(SupersedeKey {
            key: format!("ToolSearch:{}", json_string(input, "query")),
            read_path: None,
        }),
        _ => None,
    }
}

fn json_string(input: &serde_json::Value, key: &str) -> String {
    input
        .get(key)
        .map(serde_json::Value::to_string)
        .unwrap_or_else(|| "null".to_string())
}

fn input_path(input: &serde_json::Value, cwd: &Path) -> String {
    input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(|p| normalize_path(cwd, p))
        .unwrap_or_else(|| normalize_path(cwd, "."))
}

fn normalize_path(cwd: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.to_string_lossy().into_owned()
}

fn mutation_paths(pair: &ToolPair, cwd: &Path) -> Vec<String> {
    if pair.result.is_error {
        return Vec::new();
    }
    match pair.tool_use.name.as_ref() {
        crate::tools::WRITE => pair
            .tool_use
            .input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(|p| vec![normalize_path(cwd, p)])
            .unwrap_or_default(),
        crate::tools::EDIT => pair
            .tool_use
            .input
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .map(|patch| edit_paths(patch, cwd))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn edit_paths(patch: &str, cwd: &Path) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let header = line.strip_prefix('[')?.strip_suffix(']')?;
            let (path, _tag) = header.rsplit_once('#')?;
            Some(normalize_path(cwd, path))
        })
        .collect()
}

fn collect_pairs(messages: &[Message]) -> Vec<ToolPair> {
    let mut uses: HashMap<String, (usize, usize, LanguageModelToolUse)> = HashMap::new();
    for (msg_idx, message) in messages.iter().enumerate() {
        for (block_idx, content) in message.content.iter().enumerate() {
            if let MessageContent::ToolUse(tool_use) = content {
                uses.insert(tool_use.id.clone(), (msg_idx, block_idx, tool_use.clone()));
            }
        }
    }

    let mut pairs = Vec::new();
    for (msg_idx, message) in messages.iter().enumerate() {
        for (block_idx, content) in message.content.iter().enumerate() {
            if let MessageContent::ToolResult(result) = content
                && let Some((use_msg, use_block, tool_use)) = uses.get(&result.tool_use_id)
            {
                pairs.push(ToolPair {
                    use_msg: *use_msg,
                    use_block: *use_block,
                    result_msg: msg_idx,
                    result_block: block_idx,
                    tool_use: tool_use.clone(),
                    result: result.clone(),
                });
            }
        }
    }
    pairs
}

pub(crate) fn prune_for_model(
    messages: &[Message],
    cwd: &Path,
    cooldown_key: &str,
) -> Option<Vec<Message>> {
    prune_impl(messages, cwd, cooldown_key, true)
}

/// Compaction is an explicit cache boundary, so it may flush eligible cold
/// pairs without waiting for the ordinary 90-minute cooldown.
pub(crate) fn prune_for_compaction(
    messages: &[Message],
    cwd: &Path,
    cooldown_key: &str,
) -> Option<Vec<Message>> {
    let pruned = prune_impl(messages, cwd, cooldown_key, false);
    clear_cooldown(cooldown_key);
    pruned
}

/// Side-effect-free projection used by shadow metrics and the offline replay
/// harness. It applies the same hot-prefix and savings thresholds without
/// consuming or mutating the per-thread cooldown.
pub(crate) fn preview(messages: &[Message], cwd: &Path) -> Option<Vec<Message>> {
    prune_impl(messages, cwd, "preview", false)
}

pub(crate) fn clear_cooldown(cooldown_key: &str) {
    if let Some(map) = COOLDOWNS.lock().unwrap().as_mut() {
        map.remove(cooldown_key);
    }
}

fn prune_impl(
    messages: &[Message],
    cwd: &Path,
    cooldown_key: &str,
    honor_cooldown: bool,
) -> Option<Vec<Message>> {
    if honor_cooldown {
        let mut guard = COOLDOWNS.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        if map
            .get(cooldown_key)
            .is_some_and(|time| time.elapsed() < PRUNE_COOLDOWN)
        {
            return None;
        }
    }

    let pairs = collect_pairs(messages);
    let total_bytes: usize = pairs.iter().map(ToolPair::projected_bytes).sum();
    if total_bytes <= HOT_PREFIX_BYTES {
        return None;
    }

    let mut hot_bytes = 0usize;
    let mut hot_cutoff = pairs.len();
    for (idx, pair) in pairs.iter().enumerate().rev() {
        hot_bytes += pair.projected_bytes();
        hot_cutoff = idx;
        if hot_bytes >= HOT_PREFIX_BYTES {
            break;
        }
    }

    let hints: Vec<Hint> = pairs.iter().map(|pair| classify(pair, cwd)).collect();
    let mut latest_by_key: HashMap<String, usize> = HashMap::new();
    let mut latest_mutation_by_path: HashMap<String, usize> = HashMap::new();
    for (idx, (pair, hint)) in pairs.iter().zip(&hints).enumerate() {
        if let Hint::Supersede(key) = hint {
            latest_by_key.insert(key.key.clone(), idx);
        }
        for path in mutation_paths(pair, cwd) {
            latest_mutation_by_path.insert(path, idx);
        }
    }

    let mut drop_blocks: HashSet<(usize, usize)> = HashSet::new();
    let mut savings = 0usize;
    for (idx, (pair, hint)) in pairs.iter().zip(&hints).enumerate() {
        if idx >= hot_cutoff {
            continue;
        }
        let drop_pair = match hint {
            Hint::Keep => false,
            Hint::Useless => true,
            Hint::Supersede(key) => {
                let Some(&latest) = latest_by_key.get(&key.key) else {
                    continue;
                };
                if latest <= idx {
                    false
                } else if let Some(path) = &key.read_path {
                    latest_mutation_by_path
                        .get(path)
                        .is_none_or(|mutation| latest > *mutation)
                } else {
                    true
                }
            }
        };
        if drop_pair {
            drop_blocks.insert((pair.use_msg, pair.use_block));
            drop_blocks.insert((pair.result_msg, pair.result_block));
            savings += pair.projected_bytes();
        }
    }

    if savings < MIN_SAVINGS_BYTES || drop_blocks.is_empty() {
        return None;
    }

    let pruned: Vec<Message> = messages
        .iter()
        .enumerate()
        .filter_map(|(msg_idx, message)| {
            let content: Vec<MessageContent> = message
                .content
                .iter()
                .enumerate()
                .filter(|(block_idx, _)| !drop_blocks.contains(&(msg_idx, *block_idx)))
                .map(|(_, content)| content.clone())
                .collect();
            if content.is_empty() {
                return None;
            }
            Some(Message {
                id: message.id.clone(),
                timestamp: message.timestamp,
                parent_id: message.parent_id.clone(),
                role: message.role,
                content,
                ui: message.ui.clone(),
            })
        })
        .collect();

    if honor_cooldown {
        COOLDOWNS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(cooldown_key.to_string(), Instant::now());
    }
    Some(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::Role;

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> MessageContent {
        MessageContent::ToolUse(LanguageModelToolUse {
            id: id.into(),
            name: name.into(),
            raw_input: input.to_string(),
            input,
            is_input_complete: true,
            thought_signature: None,
        })
    }

    fn tool_result(id: &str, name: &str, content: String) -> MessageContent {
        MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: id.into(),
            tool_name: name.into(),
            is_error: false,
            content,
        })
    }

    fn message(role: Role, content: Vec<MessageContent>) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: 0,
            parent_id: None,
            role,
            content,
            ui: None,
        }
    }

    fn pair(id: &str, name: &str, input: serde_json::Value, output: String) -> [Message; 2] {
        [
            message(Role::Assistant, vec![tool_use(id, name, input)]),
            message(Role::User, vec![tool_result(id, name, output)]),
        ]
    }

    fn assert_protocol_pairs(messages: &[Message]) {
        let uses: HashSet<String> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                MessageContent::ToolUse(t) => Some(t.id.clone()),
                _ => None,
            })
            .collect();
        let results: HashSet<String> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                MessageContent::ToolResult(t) => Some(t.tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results, "tool protocol must remain paired");
    }

    #[test]
    fn read_key_comes_from_input_selector_not_hashline_tag() {
        let cwd = Path::new("/repo");
        let a = LanguageModelToolUse {
            id: "a".into(),
            name: crate::tools::READ.into(),
            raw_input: String::new(),
            input: serde_json::json!({"path":"src/lib.rs:1-20"}),
            is_input_complete: true,
            thought_signature: None,
        };
        let mut b = a.clone();
        b.input = serde_json::json!({"path":"src/lib.rs:21-40"});
        assert_ne!(supersede_key(&a, cwd), supersede_key(&b, cwd));
        b.input = a.input.clone();
        assert_eq!(supersede_key(&a, cwd), supersede_key(&b, cwd));
    }

    #[test]
    fn partial_prune_removes_matching_use_and_result_only() {
        let cwd = Path::new("/repo");
        let old = "old\n".repeat(24_000);
        let new = "new\n".repeat(24_000);
        let hot = "hot\n".repeat(45_000);
        let mut messages = vec![
            message(
                Role::Assistant,
                vec![
                    tool_use(
                        "read-old",
                        crate::tools::READ,
                        serde_json::json!({"path":"a.rs"}),
                    ),
                    tool_use(
                        "keep",
                        crate::tools::BASH,
                        serde_json::json!({"command":"cargo test"}),
                    ),
                ],
            ),
            message(
                Role::User,
                vec![
                    tool_result("read-old", crate::tools::READ, old),
                    tool_result("keep", crate::tools::BASH, "42 tests passed".into()),
                ],
            ),
        ];
        messages.extend(pair(
            "read-new",
            crate::tools::READ,
            serde_json::json!({"path":"a.rs"}),
            new,
        ));
        messages.extend(pair(
            "hot",
            crate::tools::BASH,
            serde_json::json!({"command":"long"}),
            hot,
        ));

        let pruned = prune_impl(&messages, cwd, "partial", false).expect("must prune");
        assert_protocol_pairs(&pruned);
        let ids: HashSet<String> = pruned
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                MessageContent::ToolUse(t) => Some(t.id.clone()),
                _ => None,
            })
            .collect();
        assert!(!ids.contains("read-old"));
        assert!(ids.contains("keep"));
        assert!(ids.contains("read-new"));
    }

    #[test]
    fn edit_requires_a_successful_refresh_before_old_read_is_removed() {
        let cwd = Path::new("/repo");
        let old = "old\n".repeat(24_000);
        let hot = "hot\n".repeat(45_000);
        let mut messages = Vec::new();
        messages.extend(pair(
            "read-old",
            crate::tools::READ,
            serde_json::json!({"path":"a.rs"}),
            old.clone(),
        ));
        messages.extend(pair(
            "edit",
            crate::tools::EDIT,
            serde_json::json!({"patch":"[/repo/a.rs#ABCD]\nSWAP 1.=1:\n+new"}),
            "Applied patch".into(),
        ));
        messages.extend(pair(
            "hot",
            crate::tools::BASH,
            serde_json::json!({"command":"long"}),
            hot.clone(),
        ));
        assert!(prune_impl(&messages, cwd, "barrier-no-refresh", false).is_none());

        messages.splice(
            4..4,
            pair(
                "read-new",
                crate::tools::READ,
                serde_json::json!({"path":"a.rs"}),
                old,
            ),
        );
        let pruned = prune_impl(&messages, cwd, "barrier-refresh", false).expect("refresh prunes");
        assert_protocol_pairs(&pruned);
        assert!(
            !pruned
                .iter()
                .flat_map(|m| &m.content)
                .any(|c| matches!(c, MessageContent::ToolUse(t) if t.id == "read-old"))
        );
    }

    #[test]
    fn failed_results_are_never_pruned() {
        let cwd = Path::new("/repo");
        let mut messages = Vec::new();
        messages.extend(pair(
            "old",
            crate::tools::READ,
            serde_json::json!({"path":"a.rs"}),
            "old\n".repeat(24_000),
        ));
        let [use_message, mut result_message] = pair(
            "failed",
            crate::tools::READ,
            serde_json::json!({"path":"a.rs"}),
            "permission denied".into(),
        );
        if let MessageContent::ToolResult(result) = &mut result_message.content[0] {
            result.is_error = true;
        }
        messages.push(use_message);
        messages.push(result_message);
        messages.extend(pair(
            "hot",
            crate::tools::BASH,
            serde_json::json!({"command":"long"}),
            "hot\n".repeat(45_000),
        ));
        assert!(prune_impl(&messages, cwd, "failed", false).is_none());
    }
}
