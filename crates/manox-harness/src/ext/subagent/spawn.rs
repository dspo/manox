//! The in-process `spawn` backend — the dsh `subagent-spawn-in-process`
//! equivalent. A child is a fresh [`AgentSession`] composed from the parent's
//! tool snapshot (stripped, filtered, and auto-deny-gated), the definition's
//! persona as system prompt, and the inherited live model. The provider owns
//! the whole one-shot run: prompt, event pump, budgets, cancellation, and
//! settlement into a [`RunResult`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::core::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use crate::core::env::ExecutionEnv;
use crate::core::ext_point_agent::{AgentDef, AgentRegistry};
use crate::core::tool::{AgentTool, ToolError};
use crate::core::types::{AgentEvent, AgentMessage, ContentBlock};
use crate::ext::prompt::base_prompt_builder;
use crate::ext::subagent::SubagentRun;
use crate::ext::subagent::provider::{RunObserver, SubagentProvider};
use crate::ext::subagent::types::{
    Capabilities, HUMAN_INTERACTION_TOOLS, ResolvedStartRequest, RunResult,
    SUBAGENT_APPROVAL_DENIED_REASON, StopReason, SubagentError, ToolFilter, validate_filter_names,
};

/// The run loop's health/cancellation tick granularity. Matches the host
/// watchdog's documented "~5s enforcement granularity".
pub const SUBAGENT_TICK: Duration = Duration::from_secs(5);

/// Bound on the session-assembly phase of a start (child build + effort
/// apply). A wedged build must fail the dispatch stage-named instead of
/// hanging the parent turn forever — the retired Steer dispatch carried the
/// same 90s guard. Worktree preparation sits outside it: its git commands
/// carry their own per-command timeouts.
pub const START_TIMEOUT: Duration = Duration::from_secs(90);

/// Cap a subagent's final summary before it rides into the parent's context
/// window.
const FINAL_MAX_BYTES: usize = 128 * 1024;
const FINAL_MAX_LINES: usize = 2000;

/// Provider-authored diagnostic cap (dsh contract: 4096 UTF-8 bytes, free of
/// tool inputs and credentials).
const DIAGNOSTIC_MAX_BYTES: usize = 4096;

/// The built-in Explore definition, embedded as a manifest.
pub fn explore_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../../ext-agents/explore.md"))
        .expect("built-in Explore manifest must parse")
}

/// The built-in Sailor definition, embedded as a manifest. General-purpose
/// coding worker with the full env-backed tool snapshot (read/write/edit +
/// bash). Unlike the read-only Explore, Sailor can modify files and run
/// commands — it is the default dispatch target for parallel
/// implementation/review/build-verification subtasks.
pub fn sailor_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../../ext-agents/sailor.md"))
        .expect("built-in Sailor manifest must parse")
}

/// Register the built-in agent definitions.
pub fn register_defaults(registry: &mut AgentRegistry) {
    registry.register(explore_agent_def());
    registry.register(sailor_agent_def());
}

/// The in-process spawn provider: composes a fresh child session per start
/// and drives it to settlement.
pub struct SpawnProvider {
    /// Snapshot of the parent's full tool set; the child's tools are a
    /// stripped/filtered subset resolved per start.
    tools: Vec<Arc<dyn AgentTool>>,
    /// Optional model runtime; without one the session is built from the
    /// default env-backed runtime.
    model_runtime: Option<ModelRuntime>,
    /// Assembly-time explicit model, the last link of the model precedence.
    model: Option<crate::core::types::Model>,
    /// Live view of the owner's current model. Subagents inherit the
    /// caller's model at dispatch time (not the assembly snapshot) so a
    /// mid-thread model switch is honored.
    model_slot: Option<Arc<Mutex<Option<crate::core::types::Model>>>>,
    /// Names the delegation tools are registered under — stripped from every
    /// child snapshot so a child cannot re-delegate (nesting is structurally
    /// disabled in this iteration).
    delegation_names: HashSet<String>,
    /// Persistent directory for subagent session transcripts. Without one
    /// the session lives in a throwaway tempdir removed on exit.
    session_dir: Option<PathBuf>,
    /// Host bridge for transcript/health surfaces.
    observer: Option<Arc<dyn RunObserver>>,
}

impl SpawnProvider {
    pub fn new(tools: Vec<Arc<dyn AgentTool>>) -> Self {
        SpawnProvider {
            tools,
            model_runtime: None,
            model: None,
            model_slot: None,
            delegation_names: HashSet::new(),
            session_dir: None,
            observer: None,
        }
    }

    /// Inject the model runtime the subagent session runs on (the caller's
    /// bridge into its own provider configuration).
    pub fn with_model_runtime(mut self, runtime: ModelRuntime) -> Self {
        self.model_runtime = Some(runtime);
        self
    }

    /// Pin the fallback model the subagent session uses.
    pub fn with_model(mut self, model: crate::core::types::Model) -> Self {
        self.model = Some(model);
        self
    }

    /// Share the owner's live model slot so dispatch inherits the caller's
    /// current model rather than the assembly-time snapshot.
    pub fn with_model_slot(mut self, slot: Arc<Mutex<Option<crate::core::types::Model>>>) -> Self {
        self.model_slot = Some(slot);
        self
    }

    /// Names delegation tools are registered under; stripped from child
    /// snapshots alongside `Steer` and the human-interaction set.
    pub fn with_delegation_names(mut self, names: HashSet<String>) -> Self {
        self.delegation_names = names;
        self
    }

    /// Persistent directory for subagent session transcripts.
    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.session_dir = Some(dir.into());
        self
    }

    /// Host bridge for transcript/health surfaces.
    pub fn with_observer(mut self, observer: Arc<dyn RunObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Resolve a child's tool set: the parent snapshot minus `Steer`, the
    /// delegation tools, and every [`HUMAN_INTERACTION_TOOLS`] name (D5),
    /// then scoped by the request's allow/deny filter, then wrapped in the
    /// `never` auto-deny gate (D6).
    fn select_child_tools(&self, filter: Option<&ToolFilter>) -> Vec<Arc<dyn AgentTool>> {
        let selected: Vec<_> = self
            .tools
            .iter()
            .filter(|t| t.name() != "Steer")
            .filter(|t| !self.delegation_names.contains(t.name()))
            .filter(|t| !HUMAN_INTERACTION_TOOLS.contains(&t.name()))
            .filter(|t| filter.is_none_or(|f| f.admits(t.name())))
            .cloned()
            .collect();
        // Definition-supplied filters are pre-sanitized at assembly (warn +
        // skip there); request-supplied filters are loud-validated at the
        // runtime before this runs — nothing to warn about here.
        auto_deny_gated(selected)
    }
}

#[async_trait]
impl SubagentProvider for SpawnProvider {
    fn name(&self) -> &str {
        "spawn"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            agent_options: true,
            output_schema: false,
            depth_limit: true,
            tool_filter: true,
            persona: true,
        }
    }

    async fn start(&self, request: ResolvedStartRequest) -> Result<SubagentRun, SubagentError> {
        let ResolvedStartRequest {
            run_id,
            inner: req,
            descriptor,
        } = request;
        // Loud unknown-name validation against the actual snapshot: a typo
        // in an allow/deny list must reject the dispatch, not silently
        // narrow (or widen) the child's tools.
        if let Some(filter) = &req.tool_filter {
            let names: HashSet<&str> = self.tools.iter().map(|t| t.name()).collect();
            validate_filter_names(filter, &names)
                .map_err(|e| SubagentError::Provider(self.name().to_string(), e.to_string()))?;
        }
        // Fail loud, not accept-then-ignore: an armed idle budget without a
        // run observer would silently never enforce.
        if self.observer.is_none() && req.budgets.idle_timeout_ms.is_some() {
            return Err(SubagentError::InvalidRequest(
                "idle_timeout was armed but this provider was assembled without a run observer                  to enforce it"
                    .to_string(),
            ));
        }
        let mut selected = self.select_child_tools(req.tool_filter.as_ref());
        let mut worktree = match req.isolation.as_deref() {
            Some("worktree") => Some(
                Worktree::prepare(req.env.as_ref(), &req.cwd)
                    .await
                    .map_err(|e| SubagentError::Provider(self.name().to_string(), e.to_string()))?,
            ),
            _ => None,
        };
        let child_cwd = worktree
            .as_ref()
            .map(|w| w.path.clone())
            .unwrap_or_else(|| req.cwd.clone());
        let (session_dir, temp_guard): (PathBuf, Option<tempfile::TempDir>) = match &self
            .session_dir
        {
            Some(dir) => (dir.clone(), None),
            None => {
                let guard = tempfile::tempdir()
                    .map_err(|e| SubagentError::Provider(self.name().to_string(), e.to_string()))?;
                (guard.path().to_path_buf(), Some(guard))
            }
        };
        let mut builder = create_agent_session()
            .with_cwd(child_cwd.clone())
            .with_session_dir(session_dir)
            .with_system_prompt_builder(base_prompt_builder(
                req.persona.clone().unwrap_or_default(),
                child_cwd.clone(),
            ))
            .with_tools(std::mem::take(&mut selected))
            // The lineage rides `metadata.subagent` (the sidebar filter and
            // usage accounting key off that object).
            .with_metadata(
                serde_json::json!({ "subagent": descriptor.metadata(req.parent_session.as_deref()) }),
            );
        if let Some(runtime) = &self.model_runtime {
            builder = builder.with_model_runtime(runtime.clone());
        }
        let inherited = self
            .model_slot
            .as_ref()
            .and_then(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).clone());
        if let Some(model) = req
            .agent_options
            .as_ref()
            .and_then(|o| o.model.clone())
            .or(inherited)
            .or_else(|| self.model.clone())
        {
            builder = builder.with_model(model);
        }
        // Bounded assembly: a wedged child build fails the dispatch
        // stage-named (and never leaks the prepared worktree) instead of
        // hanging the parent turn forever.
        let built = tokio::time::timeout(START_TIMEOUT, async {
            let mut session = builder.build().await.map_err(|e| {
                SubagentError::Provider(
                    self.name().to_string(),
                    format!("failed to start subagent: {e}"),
                )
            })?;
            if let Some(effort) = req
                .agent_options
                .as_ref()
                .and_then(|o| o.reasoning_effort.clone())
            {
                session
                    .set_thinking_level_local(Some(effort))
                    .await
                    .map_err(|e| {
                        SubagentError::Provider(
                            self.name().to_string(),
                            format!("failed to apply reasoning effort: {e}"),
                        )
                    })?;
            }
            Ok::<_, SubagentError>(session)
        })
        .await;
        let session = match built {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                if let Some(worktree) = worktree.take() {
                    let _ = worktree.clean_up(req.env.as_ref()).await;
                }
                return Err(e);
            }
            Err(_) => {
                if let Some(worktree) = worktree.take() {
                    let _ = worktree.clean_up(req.env.as_ref()).await;
                }
                return Err(SubagentError::Provider(
                    self.name().to_string(),
                    format!(
                        "subagent start timed out at stage `build-session` ({START_TIMEOUT:?})"
                    ),
                ));
            }
        };
        let handle = session.handle();
        let model = session.model().clone();

        let info = crate::ext::subagent::types::RunInfo {
            run_id: run_id.clone(),
            provider: self.name().to_string(),
            label: req.label.clone(),
            kind: req.kind.clone(),
            model: Some(model),
            local: true,
            budgets: req.budgets,
        };
        if let Some(observer) = &self.observer {
            observer.on_start(&info);
        }

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<RunResult>();
        let dispose = req.cancel.clone();
        let run_dispose = dispose.clone();
        let observer = self.observer.clone();
        let provider_name = self.name().to_string();
        let label = req.label.clone();
        let budgets = req.budgets;
        let task_run_id = run_id.clone();
        let task_provider = provider_name.clone();
        let task_label = label.clone();
        tokio::spawn(async move {
            let _temp_guard = temp_guard; // hold the throwaway transcript dir alive
            // Panic-tolerant settle: a run-loop panic must still settle the
            // result and the observer's surfaces (otherwise the rail row
            // stays Running forever), not take the task down silently.
            let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(drive_run(
                DrivenRun {
                    session,
                    handle,
                    prompt: req.prompt,
                    budgets,
                    dispose: dispose.clone(),
                    observer: observer.clone(),
                    run_id: task_run_id.clone(),
                    provider_name: task_provider.clone(),
                    label: task_label.clone(),
                    worktree,
                    env: req.env,
                },
            )))
            .await
            .unwrap_or_else(|_| {
                let result = RunResult {
                    output: String::new(),
                    structured: None,
                    diagnostic: Some(cap_diagnostic("subagent run task panicked")),
                    stop_reason: StopReason::Error,
                };
                if let Some(observer) = &observer {
                    observer.on_settled(&crate::ext::subagent::types::RunEndInfo {
                        run_id: task_run_id,
                        provider: task_provider,
                        label: task_label,
                        local: true,
                        stop_reason: result.stop_reason.clone(),
                        output: String::new(),
                    });
                }
                result
            });
            let _ = result_tx.send(result);
        });
        let result_future = async move {
            result_rx.await.unwrap_or_else(|_| RunResult {
                output: String::new(),
                structured: None,
                diagnostic: Some("subagent run task died".to_string()),
                stop_reason: StopReason::Error,
            })
        };
        Ok(SubagentRun::new(
            run_id,
            run_dispose,
            Box::pin(result_future),
        ))
    }
}

/// Everything the run-loop task owns for one child. Collected so the loop
/// body stays readable: the task owns the session (and its tempdir guard),
/// folds child events through the observer, honors the dispose token and the
/// wall-clock budget, and settles into a [`RunResult`].
struct DrivenRun {
    session: AgentSession,
    handle: crate::core::harness::HarnessHandle,
    prompt: String,
    budgets: crate::ext::subagent::types::Budgets,
    dispose: CancellationToken,
    observer: Option<Arc<dyn RunObserver>>,
    run_id: String,
    provider_name: String,
    label: String,
    worktree: Option<Worktree>,
    env: Arc<dyn ExecutionEnv>,
}

/// How the run loop exited, before settlement mapping.
enum Ending {
    /// The prompt future returned.
    Done(Result<Vec<AgentMessage>, anyhow::Error>),
    /// The dispose token fired (interrupt / parent abort).
    Aborted,
    /// The wall-clock budget expired.
    TimedOut,
    /// The observer enforced a stall (armed idle budget).
    Stalled,
}

async fn drive_run(run: DrivenRun) -> RunResult {
    let DrivenRun {
        mut session,
        handle,
        prompt,
        budgets,
        dispose,
        observer,
        run_id,
        provider_name,
        label,
        mut worktree,
        env,
    } = run;
    // Bridge the child session's streamed events to the observer so the
    // transcript rail + health watchdog track live dynamics.
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let _subscription = session.subscribe(Arc::new(move |event, _cancel| {
        let _ = ev_tx.send(event);
        Box::pin(async move {})
    }));
    let full_prompt = format!(
        "{}\n\nWhen done, end your turn with a concise summary: what you \
         changed (files + intent), what you ran (commands + outcomes), and \
         the final result.",
        prompt
    );
    let mut prompt_fut = Box::pin(session.prompt(&full_prompt));
    // Wall-clock deadline: an armed budget gets a real sleep; an unbounded
    // run parks a never-ready branch in its place so the loop stays uniform.
    let deadline = match budgets.timeout_ms {
        Some(ms) => futures::future::Either::Left(tokio::time::sleep(Duration::from_millis(ms))),
        None => futures::future::Either::Right(std::future::pending::<()>()),
    };
    tokio::pin!(deadline);
    let mut watchdog_tick = tokio::time::interval(SUBAGENT_TICK);
    watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Partial-output salvage, read inside the killing arms right after the
    // prompt future is dropped (the borrow must provably end first).
    let mut partial = String::new();
    let ending = loop {
        tokio::select! {
            r = prompt_fut.as_mut() => break Ending::Done(r),
            Some(event) = ev_rx.recv() => {
                if let Some(observer) = &observer {
                    observer.on_child_event(&run_id, &event);
                }
            }
            _ = dispose.cancelled() => {
                drop(prompt_fut);
                handle.abort();
                break Ending::Aborted;
            }
            // Budget expired: a prompt that finished in the same instant the
            // budget expired is a success, not a timeout — re-check before
            // killing (select! picks a ready branch at random).
            () = deadline.as_mut() => {
                if let std::task::Poll::Ready(r) = futures::poll!(prompt_fut.as_mut()) {
                    break Ending::Done(r);
                }
                drop(prompt_fut);
                partial = truncate_final(&extract_final_text(session.harness_messages()));
                handle.abort();
                break Ending::TimedOut;
            }
            _ = watchdog_tick.tick() => {
                let enforce = observer.as_ref().is_some_and(|o| o.on_tick(&run_id));
                if enforce {
                    drop(prompt_fut);
                    partial = truncate_final(&extract_final_text(session.harness_messages()));
                    handle.abort();
                    break Ending::Stalled;
                }
            }
        }
    };
    // Clean up the worktree; a kept (non-pristine) worktree's note must ride
    // the result so the caller can find the edits.
    let kept_note = match worktree.take() {
        Some(wt) => match wt.clean_up(env.as_ref()).await {
            Some(kept) => format!("\n\n[worktree kept: {kept}]"),
            None => String::new(),
        },
        None => String::new(),
    };
    // `partial` carries whatever the child produced before a killed ending,
    // so the parent keeps the work; a natural completion reads its output
    // from the returned messages instead.
    let result = match ending {
        Ending::Done(Ok(messages)) => RunResult {
            output: format!(
                "{}{kept_note}",
                truncate_final(&extract_final_text(&messages))
            ),
            structured: None,
            diagnostic: None,
            stop_reason: StopReason::Completed,
        },
        Ending::Done(Err(e)) if e.to_string().contains("aborted") => RunResult {
            output: String::new(),
            structured: None,
            diagnostic: Some(cap_diagnostic("aborted")),
            stop_reason: StopReason::Aborted,
        },
        Ending::Done(Err(e)) => RunResult {
            output: partial,
            structured: None,
            diagnostic: Some(cap_diagnostic(&format!("subagent failed: {e}{kept_note}"))),
            stop_reason: StopReason::Error,
        },
        Ending::Aborted => RunResult {
            output: String::new(),
            structured: None,
            diagnostic: Some(cap_diagnostic(&format!("aborted{kept_note}"))),
            stop_reason: StopReason::Aborted,
        },
        Ending::TimedOut => RunResult {
            output: partial,
            structured: None,
            diagnostic: Some(cap_diagnostic(&format!(
                "subagent timed out: budget {}ms exceeded{kept_note}",
                budgets.timeout_ms.unwrap_or_default()
            ))),
            stop_reason: StopReason::Error,
        },
        Ending::Stalled => RunResult {
            output: partial,
            structured: None,
            diagnostic: Some(cap_diagnostic(&format!(
                "subagent timed out: stalled — idle budget {}ms exceeded{kept_note}",
                budgets.idle_timeout_ms.unwrap_or_default()
            ))),
            stop_reason: StopReason::Error,
        },
    };
    if let Some(observer) = &observer {
        observer.on_settled(&crate::ext::subagent::types::RunEndInfo {
            run_id,
            provider: provider_name,
            label,
            local: true,
            stop_reason: result.stop_reason.clone(),
            output: result.output.clone(),
        });
    }
    result
}

/// Extract the final assistant text: the text blocks of the last assistant
/// message that carried content, skipping empty/usage-only messages — the
/// dsh `finalAssistantOutput` canonical rule.
pub fn extract_final_text(messages: &[AgentMessage]) -> String {
    for msg in messages.iter().rev() {
        if let AgentMessage::Assistant { content, .. } = msg {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn truncate_final(text: &str) -> String {
    let by_lines: String = text
        .lines()
        .take(FINAL_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if by_lines.len() > FINAL_MAX_BYTES {
        by_lines.chars().take(FINAL_MAX_BYTES).collect()
    } else {
        by_lines
    }
}

fn cap_diagnostic(text: &str) -> String {
    if text.len() <= DIAGNOSTIC_MAX_BYTES {
        text.to_string()
    } else {
        text.chars().take(DIAGNOSTIC_MAX_BYTES).collect()
    }
}

/// An isolated git worktree a subagent runs in when the dispatch passes
/// `isolation: "worktree"`. Created at dispatch time on a throwaway branch
/// under the system temp dir; the subagent session's cwd is the worktree
/// path, so every tool operates inside it without manual `cd`. `clean_up`
/// auto-removes a pristine worktree (no commits, no uncommitted changes)
/// and its branch; a worktree with work is kept and its branch + path are
/// reported back so the caller never silently loses edits.
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
    /// The repo HEAD SHA captured at `prepare` time; `clean_up` compares the
    /// branch tip against it to detect committed work.
    pub base: String,
}

impl Worktree {
    pub async fn prepare(env: &dyn ExecutionEnv, cwd: &Path) -> Result<Self, ToolError> {
        let repo = cwd.to_path_buf();
        let suffix = unique_suffix();
        let branch = format!("sailor-{suffix}");
        let path = std::env::temp_dir().join(format!("manox-sailor-{suffix}"));
        let cmd = format!(
            "git -C {repo_q} rev-parse --verify HEAD",
            repo_q = shell_quote(&repo),
        );
        let base = env
            .exec(&cmd, Duration::from_secs(10), CancellationToken::new())
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("git rev-parse HEAD: {e}")))?;
        if base.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "cannot resolve repo HEAD (exit {}): {}",
                base.exit_code,
                base.stderr.trim()
            )));
        }
        let base = base.stdout.trim().to_string();
        let cmd = format!(
            "git -C {repo_q} worktree add {path_q} -b {branch} {base}",
            repo_q = shell_quote(&repo),
            path_q = shell_quote(&path),
        );
        let res = env
            .exec(&cmd, Duration::from_secs(30), CancellationToken::new())
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("git worktree add: {e}")))?;
        if res.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "git worktree add failed (exit {}): {}",
                res.exit_code,
                res.stderr.trim()
            )));
        }
        Ok(Worktree {
            path,
            branch,
            repo,
            base,
        })
    }

    /// Remove a pristine worktree + branch; keep a worktree with work and
    /// return a message naming the kept branch + path. Best-effort: a git
    /// failure during the pristine check falls back to keeping the worktree
    /// (never destroy work silently).
    pub async fn clean_up(self, env: &dyn ExecutionEnv) -> Option<String> {
        if self.is_pristine(env).await {
            let rm = format!(
                "git -C {repo_q} worktree remove {path_q}",
                repo_q = shell_quote(&self.repo),
                path_q = shell_quote(&self.path),
            );
            let _ = env
                .exec(&rm, Duration::from_secs(30), CancellationToken::new())
                .await;
            let del = format!(
                "git -C {repo_q} branch -D {branch}",
                repo_q = shell_quote(&self.repo),
                branch = self.branch,
            );
            let _ = env
                .exec(&del, Duration::from_secs(10), CancellationToken::new())
                .await;
            let _ = std::fs::remove_dir(&self.path);
            None
        } else {
            Some(format!(
                "[worktree kept: branch={}, path={}]",
                self.branch,
                self.path.display()
            ))
        }
    }

    /// Pristine = no commits beyond the dispatch base AND no uncommitted
    /// changes in the worktree. Either check failing (git error) is treated
    /// as not-pristine so the worktree is kept rather than destroyed.
    async fn is_pristine(&self, env: &dyn ExecutionEnv) -> bool {
        let commits = format!(
            "git -C {repo_q} rev-list --count {base}..{branch}",
            repo_q = shell_quote(&self.repo),
            base = self.base.as_str(),
            branch = self.branch.as_str(),
        );
        let res = env
            .exec(&commits, Duration::from_secs(10), CancellationToken::new())
            .await;
        let committed = res
            .ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout.trim().parse::<u64>().unwrap_or(1))
            .unwrap_or(1);
        if committed > 0 {
            return false;
        }
        let dirty = format!(
            "git -C {path_q} status --porcelain",
            path_q = shell_quote(&self.path)
        );
        let res = env
            .exec(&dirty, Duration::from_secs(10), CancellationToken::new())
            .await;
        res.ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout.trim().is_empty())
            .unwrap_or(false)
    }
}

/// A dependency-free uniqueness suffix for parallel worktree branch names:
/// pid + the full nanosecond timestamp. An atomic counter guarantees no two
/// dispatches in the same process collide even if the clock stalls.
fn unique_suffix() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// Single-quote a path for a shell command string with the standard `'\''`
/// escape, so a cwd containing `$`, backticks, `"`, or `'` cannot break or
/// inject into the command.
fn shell_quote(path: &Path) -> String {
    let s = path.display().to_string();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The model-facing reason a gated tool is rejected inside a subagent is
/// [`SUBAGENT_APPROVAL_DENIED_REASON`]; this gate is its enforcement.
///
/// Wraps a snapshot tool with the subagent's `never` approval policy. A tool
/// whose `requires_approval(params)` is `true` settles to an error the model
/// sees instead of executing un-gated. This is the delegation-boundary gate
/// the subagent session otherwise lacks: the host `ApprovalGatedTool` is
/// composed only on the main assembly line, so a child session would
/// otherwise run bare gated tools with nothing to consult.
pub struct SubagentAutoDenyGate {
    inner: Arc<dyn AgentTool>,
}

impl SubagentAutoDenyGate {
    fn new(inner: Arc<dyn AgentTool>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl AgentTool for SubagentAutoDenyGate {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        // Preserve the declarative hint so introspection (and the invariant
        // test) still sees that this tool is approval-bearing.
        self.inner.requires_approval(params)
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn execution_mode(&self) -> crate::core::tool::ExecutionMode {
        self.inner.execution_mode()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn crate::core::tool::ToolContext,
    ) -> Result<crate::core::tool::AgentToolResult, ToolError> {
        if self.inner.requires_approval(&params) {
            return Err(ToolError::ExecutionFailed(
                SUBAGENT_APPROVAL_DENIED_REASON.to_string(),
            ));
        }
        self.inner.execute(tool_call_id, params, signal, ctx).await
    }
    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn crate::core::tool::ToolContext,
        progress: &dyn crate::core::tool::ToolProgress,
    ) -> Result<crate::core::tool::AgentToolResult, ToolError> {
        if self.inner.requires_approval(&params) {
            return Err(ToolError::ExecutionFailed(
                SUBAGENT_APPROVAL_DENIED_REASON.to_string(),
            ));
        }
        self.inner
            .execute_with_progress(tool_call_id, params, signal, ctx, progress)
            .await
    }
}

/// Tool names the subagent gate is known to wrap even when `is_read_only`
/// would not (defensive: a mutating tool that also, incorrectly, declares
/// itself read-only still cannot bypass the `never` boundary). Bash is the
/// salient case — its `requires_approval` is params-aware (escalated
/// out-of-sandbox commands) while it must stay available for in-workspace
/// commands.
const SUBAGENT_APPROVAL_BEARING: &[&str] = &["Bash"];

/// Map a snapshot through the subagent `never` policy: wrap each tool that
/// can require approval (any mutating tool, plus the names in
/// [`SUBAGENT_APPROVAL_BEARING`]), pass pure reads through untouched. The
/// wrapper consults `requires_approval` per call, so a workspace-confined
/// write that answers `false` still runs while an approval-bearing call is
/// rejected.
pub fn auto_deny_gated(tools: Vec<Arc<dyn AgentTool>>) -> Vec<Arc<dyn AgentTool>> {
    tools
        .into_iter()
        .map(|t| {
            if !t.is_read_only() || SUBAGENT_APPROVAL_BEARING.contains(&t.name()) {
                Arc::new(SubagentAutoDenyGate::new(t)) as Arc<dyn AgentTool>
            } else {
                t
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool::{AgentToolResult, LocalToolContext, ToolState};
    use crate::ext::subagent::types::Budgets;

    #[test]
    fn explore_manifest_parses() {
        let def = explore_agent_def();
        assert_eq!(def.name, "Explore");
        assert_eq!(def.tools, vec!["Read", "Grep", "Glob", "Ls"]);
        assert!(def.system_prompt.contains("read-only codebase"));
        assert!(def.description.to_lowercase().contains("read-only"));
    }

    #[test]
    fn sailor_manifest_parses() {
        let def = sailor_agent_def();
        assert_eq!(def.name, "Sailor");
        assert!(def.tools.is_empty(), "empty tools means full snapshot");
        assert!(def.model.is_none(), "inherits the Captain's model");
        assert!(
            def.system_prompt.contains("concise summary"),
            "system prompt requires a summary on completion"
        );
    }

    fn def_tools_snapshot() -> Vec<Arc<dyn AgentTool>> {
        vec![
            Arc::new(crate::core::tools::read::ReadTool),
            Arc::new(crate::core::tools::write::WriteTool),
        ]
    }

    fn provider_with(tools: Vec<Arc<dyn AgentTool>>, delegation: &[&str]) -> SpawnProvider {
        SpawnProvider::new(tools)
            .with_delegation_names(delegation.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn select_child_tools_filters_by_allow_list() {
        let provider = provider_with(def_tools_snapshot(), &[]);
        let filter = ToolFilter {
            allow: vec!["Read".into()],
            deny: vec![],
        };
        let selected = provider.select_child_tools(Some(&filter));
        assert_eq!(
            selected.iter().map(|t| t.name()).collect::<Vec<_>>(),
            vec!["Read"]
        );
    }

    #[test]
    fn select_child_tools_empty_filter_means_full_snapshot() {
        let provider = provider_with(def_tools_snapshot(), &[]);
        let selected = provider.select_child_tools(None);
        assert_eq!(selected.len(), 2);
    }

    /// D5 invariant: a child never receives `Steer`, a registered delegation
    /// tool, or a human-interaction tool — whether via the full snapshot or
    /// by a filter naming it explicitly.
    #[test]
    fn select_child_tools_strips_privileged_and_human_interaction_tools() {
        struct NamedTool(&'static str, bool);
        #[async_trait::async_trait]
        impl AgentTool for NamedTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn is_read_only(&self) -> bool {
                self.1
            }
            async fn execute(
                &self,
                _: &str,
                _: serde_json::Value,
                _: CancellationToken,
                _: &dyn crate::core::tool::ToolContext,
            ) -> Result<AgentToolResult, ToolError> {
                Ok(AgentToolResult::text("ran"))
            }
        }
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(NamedTool("Steer", false)),
            Arc::new(NamedTool("AskUserQuestion", true)),
            Arc::new(NamedTool("Sailor", false)),
            Arc::new(NamedTool("Read", true)),
        ];
        let provider = provider_with(tools, &["Sailor"]);
        let selected = provider.select_child_tools(None);
        assert_eq!(
            selected.iter().map(|t| t.name()).collect::<Vec<_>>(),
            vec!["Read"],
            "full-snapshot child keeps only the plain read"
        );
    }

    /// A minimal mock whose `requires_approval` is driven by a `gate` param —
    /// enough to exercise the D6 `never` gate without a real tool.
    struct MockGateTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait::async_trait]
    impl AgentTool for MockGateTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn requires_approval(&self, params: &serde_json::Value) -> bool {
            params
                .get("gate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn crate::core::tool::ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            Ok(AgentToolResult::text(format!("RAN:{}", self.name)))
        }
    }

    /// D6 invariant: the subagent `never` gate rejects an approval-bearing
    /// call with the dsh reason and passes an approval-free call through.
    #[tokio::test]
    async fn auto_deny_gate_rejects_gated_and_runs_free() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LocalToolContext::new(
            Arc::new(crate::core::env::TokioExecutionEnv::new(
                dir.path().to_path_buf(),
            )),
            dir.path().to_path_buf(),
            Arc::new(ToolState::new()),
        );

        let wrapped = auto_deny_gated(vec![Arc::new(MockGateTool {
            name: "Gated",
            read_only: false,
        })]);
        assert_eq!(wrapped.len(), 1);
        assert!(
            !wrapped[0].is_read_only(),
            "wrapping preserves read-only=false"
        );

        let gated = wrapped[0]
            .execute(
                "c1",
                serde_json::json!({ "gate": true }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .expect_err("a gated call must not run in a subagent");
        assert!(
            gated.to_string().contains("rejected automatically"),
            "the denial names the dsh boundary: {gated}"
        );

        let free = wrapped[0]
            .execute(
                "c2",
                serde_json::json!({ "gate": false }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .expect("an approval-free call runs");
        assert!(!free.is_error);
    }

    /// A pure read passes through unwrapped; Bash is gated defensively by
    /// name even when it declares read-only.
    #[tokio::test]
    async fn auto_deny_gate_leaves_reads_untouched_but_gates_bash() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LocalToolContext::new(
            Arc::new(crate::core::env::TokioExecutionEnv::new(
                dir.path().to_path_buf(),
            )),
            dir.path().to_path_buf(),
            Arc::new(ToolState::new()),
        );

        let read_like = auto_deny_gated(vec![Arc::new(MockGateTool {
            name: "Grep",
            read_only: true,
        })]);
        let ran = read_like[0]
            .execute(
                "c1",
                serde_json::json!({ "gate": true }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .expect("an unwrapped read runs regardless of gate");
        assert!(!ran.is_error);

        let bash = auto_deny_gated(vec![Arc::new(MockGateTool {
            name: "Bash",
            read_only: true,
        })]);
        let denied = bash[0]
            .execute(
                "c2",
                serde_json::json!({ "gate": true }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .expect_err("Bash rides the gate even when it claims read-only");
        assert!(denied.to_string().contains("rejected automatically"));
    }

    /// Real-git round-trip: `Worktree::prepare` creates a worktree on a
    /// throwaway branch; `clean_up` removes a pristine one and its branch.
    #[tokio::test]
    async fn worktree_prepare_and_clean_up_round_trip() {
        let dir = tempfile::tempdir().expect("temp repo dir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git runs")
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"][..],
            &["config", "user.name", "t"][..],
            &["commit", "-q", "-m", "init", "--allow-empty"][..],
        ] {
            assert!(git(args).status.success(), "git {:?} failed", args);
        }
        let env = crate::core::env::TokioExecutionEnv::new(repo.to_path_buf());
        let wt = Worktree::prepare(&env, repo)
            .await
            .expect("worktree prepares");
        assert!(wt.path.is_dir());
        assert!(wt.path.join(".git").is_file(), "linked worktree");
        let path = wt.path.clone();
        let branch = wt.branch.clone();
        let kept = wt.clean_up(&env).await;
        assert!(kept.is_none(), "pristine worktree is removed");
        assert!(!path.exists(), "worktree removed after clean_up");
        let branches = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["branch", "--list"])
                .output()
                .expect("git branch --list")
                .stdout,
        )
        .to_string();
        assert!(!branches.contains(&branch), "branch {branch} deleted");
    }

    /// B2 invariant: a worktree with committed work is kept (never
    /// destroyed) and its branch + path are reported back.
    #[tokio::test]
    async fn worktree_with_commits_is_kept_not_destroyed() {
        let dir = tempfile::tempdir().expect("temp repo dir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git runs")
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"][..],
            &["config", "user.name", "t"][..],
            &["commit", "-q", "-m", "init", "--allow-empty"][..],
        ] {
            assert!(git(args).status.success(), "git {:?} failed", args);
        }
        let env = crate::core::env::TokioExecutionEnv::new(repo.to_path_buf());
        let wt = Worktree::prepare(&env, repo)
            .await
            .expect("worktree prepares");
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .args(["commit", "-q", "-m", "sailor work", "--allow-empty"])
            .output()
            .expect("commit in worktree");
        assert!(commit.status.success());

        let path = wt.path.clone();
        let branch = wt.branch.clone();
        let kept = wt.clean_up(&env).await;
        assert!(kept.is_some(), "a worktree with commits is kept");
        assert!(kept.as_deref().unwrap().contains(&branch));
        assert!(path.exists());
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .output();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "-D", &branch])
            .output();
    }

    /// Output extraction: the last non-empty assistant message wins; an
    /// empty transcript yields nothing (the dsh canonical rule).
    #[test]
    fn extract_final_text_prefers_last_non_empty_assistant() {
        use crate::core::types::{AgentMessage, ContentBlock};
        let text_block = |t: &str| {
            vec![ContentBlock::Text {
                text: t.to_string(),
                signature: None,
            }]
        };
        let messages = vec![
            AgentMessage::Assistant {
                content: text_block("intermediate narration"),
                model: "m".into(),
                provider: "p".into(),
                api: "a".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: None,
                usage: Box::new(crate::core::types::Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::User {
                content: Vec::new(),
                timestamp: chrono::Utc::now(),
                id: None,
            },
            AgentMessage::Assistant {
                content: text_block("the final answer"),
                model: "m".into(),
                provider: "p".into(),
                api: "a".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: None,
                usage: Box::new(crate::core::types::Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        assert_eq!(extract_final_text(&messages), "the final answer");
        assert_eq!(extract_final_text(&[]), "");
    }

    /// Budget validation: below-minimum arms are input errors.
    #[test]
    fn budgets_below_minimum_rejected() {
        assert!(Budgets::default().validate().is_ok());
        let err = Budgets {
            timeout_ms: Some(500),
            idle_timeout_ms: None,
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("at least 1000ms"), "{err}");
        let err = Budgets {
            timeout_ms: None,
            idle_timeout_ms: Some(999),
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("idle_timeout"), "{err}");
    }
}
