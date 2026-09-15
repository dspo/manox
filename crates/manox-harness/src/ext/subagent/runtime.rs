//! [`SubagentRuntime`] — the provider registry and one-shot start seam (the
//! dsh `ctx.subagents` service, in-process scope). Providers register with a
//! guard handle whose drop removes them (effect-scoped, HMR-safe in dsh
//! terms); `start` validates capabilities fail-loud, resolves the durable
//! child descriptor, and dispatches to the chosen provider. The runtime also
//! tracks live runs for the `list_agents`/`interrupt_agent` control surface.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::ext::subagent::SubagentRun;
use crate::ext::subagent::descriptor::Descriptor;
use crate::ext::subagent::provider::SubagentProvider;
use crate::ext::subagent::types::{
    Event, ResolvedStartRequest, RunEndInfo, RunInfo, RunSnapshot, StartRequest, SubagentError,
    assert_capabilities,
};

/// One live run tracked between publication and settlement.
struct ActiveRun {
    snapshot: RunSnapshot,
    dispose: CancellationToken,
    started_at: Instant,
}

/// The subagent service: registry + capability-checked starts + live-run
/// table + the `start`/`end` observation broadcast. One per thread assembly.
pub struct SubagentRuntime {
    providers: RwLock<BTreeMap<String, Arc<dyn SubagentProvider>>>,
    runs: Mutex<BTreeMap<String, ActiveRun>>,
    /// Tool names the model-facing delegation tools are registered under.
    /// Child snapshots strip every one of them (nesting is structurally
    /// disabled in this iteration; the depth fields validate the budget at
    /// start and ride the descriptor for the durable lineage).
    delegation_tools: Mutex<BTreeSet<String>>,
    events_tx: broadcast::Sender<Event>,
    run_seq: AtomicU64,
}

impl SubagentRuntime {
    pub fn new() -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(64);
        Arc::new(SubagentRuntime {
            providers: RwLock::new(BTreeMap::new()),
            runs: Mutex::new(BTreeMap::new()),
            delegation_tools: Mutex::new(BTreeSet::new()),
            events_tx,
            run_seq: AtomicU64::new(0),
        })
    }

    /// Register a provider; the returned guard removes it on drop.
    pub fn register_provider(
        self: &Arc<Self>,
        provider: Arc<dyn SubagentProvider>,
    ) -> ProviderGuard {
        let name = provider.name().to_string();
        write(&self.providers).insert(name.clone(), provider);
        ProviderGuard {
            runtime: Arc::downgrade(self),
            name,
        }
    }

    /// Register a provider for the runtime's whole lifetime. For
    /// engine-scoped runtimes whose lifetime already encloses every
    /// provider's (each session assembly builds its own runtime), where an
    /// effect-scoped guard would only add a drop-order hazard.
    pub fn register_provider_permanent(&self, provider: Arc<dyn SubagentProvider>) {
        write(&self.providers).insert(provider.name().to_string(), provider);
    }

    pub fn get_provider(&self, name: &str) -> Result<Arc<dyn SubagentProvider>, SubagentError> {
        read(&self.providers)
            .get(name)
            .cloned()
            .ok_or_else(|| SubagentError::UnknownProvider(name.to_string()))
    }

    pub fn list_providers(&self) -> Vec<String> {
        read(&self.providers).keys().cloned().collect()
    }

    /// The observation feed (`start`/`end` pairs, one per run).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    /// Reserve a delegation tool name so child snapshots strip it.
    pub fn register_delegation_tool(&self, name: &str) {
        mutex(&self.delegation_tools).insert(name.to_string());
    }

    /// Names of every registered delegation tool (the child-snapshot strip
    /// set).
    pub fn delegation_tool_names(&self) -> HashSet<String> {
        mutex(&self.delegation_tools).iter().cloned().collect()
    }

    /// Validate the request against the named provider and dispatch a
    /// ONE-SHOT child. Fail-loud: an unknown provider, an unsupported
    /// capability, an under-minimum budget, or an exhausted depth budget
    /// rejects before any provider work begins.
    pub async fn start(
        self: &Arc<Self>,
        provider: &str,
        request: StartRequest,
    ) -> Result<SubagentRun, SubagentError> {
        let provider_impl = self.get_provider(provider)?;
        request.budgets.validate()?;
        assert_capabilities(provider, &provider_impl.capabilities(), &request)?;
        let child_depth = request.parent_depth + 1;
        if let Some(max) = request.max_depth.filter(|&max| child_depth > max) {
            return Err(SubagentError::InvalidRequest(format!(
                "delegation depth budget exhausted: child would run at depth {child_depth} \
                 but the cap is {max}"
            )));
        }
        let mut descriptor = Descriptor::one_shot(
            provider,
            request.label.clone(),
            request.kind.clone(),
            child_depth,
        );
        descriptor.persona = request.persona.clone();
        descriptor.tool_filter = request.tool_filter.clone();

        let run_id = format!("sub-{}", self.run_seq.fetch_add(1, Ordering::SeqCst));
        let label = request.label.clone();
        let request_kind = request.kind.clone();
        let request_model = request.agent_options.as_ref().and_then(|o| o.model.clone());
        let resolved_budgets = request.budgets;
        let resolved = ResolvedStartRequest {
            run_id: run_id.clone(),
            inner: request,
            descriptor,
        };
        let run = provider_impl.start(resolved).await?;
        mutex(&self.runs).insert(
            run_id.clone(),
            ActiveRun {
                snapshot: RunSnapshot {
                    id: run_id.clone(),
                    label: label.clone(),
                    provider: provider.to_string(),
                    status: "running",
                    running_for_ms: 0,
                    local: true,
                },
                dispose: run.dispose_token().clone(),
                started_at: Instant::now(),
            },
        );
        let _ = self.events_tx.send(Event::Start(RunInfo {
            run_id: run_id.clone(),
            provider: provider.to_string(),
            label: label.clone(),
            kind: request_kind,
            model: request_model,
            local: true,
            budgets: resolved_budgets,
        }));
        // Settle watcher: close the run table entry and publish the paired
        // end event. The run's own consumers await `run.result` separately.
        let watcher_runtime = Arc::downgrade(self);
        let watcher_id = run_id.clone();
        let watcher_provider = provider.to_string();
        let watcher_label = label;
        let watcher_result = run.clone_result_future();
        tokio::spawn(async move {
            let result = watcher_result.await;
            if let Some(runtime) = watcher_runtime.upgrade() {
                mutex(&runtime.runs).remove(&watcher_id);
                let _ = runtime.events_tx.send(Event::End(RunEndInfo {
                    run_id: watcher_id,
                    provider: watcher_provider,
                    label: watcher_label,
                    local: true,
                    stop_reason: result.stop_reason.clone(),
                    output: result.output.clone(),
                }));
            }
        });
        Ok(run)
    }

    /// Reserved: continuable children. No in-process backend implements the
    /// continuable capability yet, so this always rejects — the seam exists
    /// so the future manager lands without a second start path.
    pub async fn start_continuable(
        &self,
        provider: &str,
        _request: StartRequest,
    ) -> Result<(), SubagentError> {
        let provider_impl = self.get_provider(provider)?;
        if provider_impl.supports_continuable() {
            // A continuable-capable backend still has no manager to drive
            // the durable child lifecycle in this iteration.
            return Err(SubagentError::Provider(
                provider.to_string(),
                "the continuable manager is not assembled in this build".to_string(),
            ));
        }
        Err(SubagentError::UnsupportedCapability {
            provider: provider.to_string(),
            capability: "continuable",
        })
    }

    /// Snapshot the live runs for the `list_agents` control tool.
    pub fn list_runs(&self) -> Vec<RunSnapshot> {
        let now = Instant::now();
        mutex(&self.runs)
            .values()
            .map(|run| RunSnapshot {
                running_for_ms: now.saturating_duration_since(run.started_at).as_millis() as u64,
                ..run.snapshot.clone()
            })
            .collect()
    }

    /// Cancel one live run; `true` when the id named a live child.
    pub fn interrupt(&self, run_id: &str) -> bool {
        mutex(&self.runs)
            .get(run_id)
            .map(|run| {
                run.dispose.cancel();
                true
            })
            .unwrap_or(false)
    }

    /// Whether `run_id` names a live child of this runtime.
    pub fn is_live(&self, run_id: &str) -> bool {
        mutex(&self.runs).contains_key(run_id)
    }
}

/// Poison-tolerant lock: a holder that panicked must not take every later
/// dispatch down with a secondary panic.
fn read<T>(rwlock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    rwlock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(rwlock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rwlock.write().unwrap_or_else(|e| e.into_inner())
}

fn mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Removes the provider again when dropped (effect-scoped registration).
pub struct ProviderGuard {
    runtime: Weak<SubagentRuntime>,
    name: String,
}

impl Drop for ProviderGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade() {
            write(&runtime.providers).remove(&self.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::subagent::SubagentRun;
    use crate::ext::subagent::types::{Capabilities, RunResult, StopReason, test_request};
    use std::sync::atomic::AtomicU64;

    /// A scripted provider: records capability checks, settles immediately
    /// with a canned result, and reports whether it was asked to prepare a
    /// continuable child.
    struct ScriptedProvider {
        caps: Capabilities,
        continuable: bool,
        started: AtomicU64,
    }

    impl ScriptedProvider {
        fn new(caps: Capabilities) -> Self {
            ScriptedProvider {
                caps,
                continuable: false,
                started: AtomicU64::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl SubagentProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        async fn start(&self, request: ResolvedStartRequest) -> Result<SubagentRun, SubagentError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            assert!(
                request.run_id.starts_with("sub-"),
                "the runtime mints the run id"
            );
            assert_eq!(request.descriptor.provider, "scripted");
            assert_eq!(request.descriptor.depth, request.inner.parent_depth + 1);
            let (tx, rx) = tokio::sync::oneshot::channel();
            tx.send(RunResult {
                output: "done".into(),
                structured: None,
                diagnostic: None,
                stop_reason: StopReason::Completed,
            })
            .unwrap();
            Ok(SubagentRun::new(
                request.run_id,
                request.inner.cancel.clone(),
                Box::pin(async move { rx.await.unwrap() }),
            ))
        }
        fn supports_continuable(&self) -> bool {
            self.continuable
        }
    }

    fn full_caps() -> Capabilities {
        Capabilities {
            agent_options: true,
            output_schema: true,
            depth_limit: true,
            tool_filter: true,
            persona: true,
        }
    }

    #[tokio::test]
    async fn start_happy_path_publishes_and_settles_events() {
        let runtime = SubagentRuntime::new();
        let _guard = runtime.register_provider(Arc::new(ScriptedProvider::new(full_caps())));
        let mut events = runtime.subscribe();
        let run = runtime
            .start("scripted", test_request("task label", "do it"))
            .await
            .expect("start");
        let result = run.result().await;
        assert_eq!(result.output, "done");
        // Start event first, end event after settlement.
        assert!(matches!(events.recv().await, Ok(Event::Start(_))));
        match events.recv().await {
            Ok(Event::End(end)) => {
                assert_eq!(end.stop_reason, StopReason::Completed);
                assert_eq!(end.output, "done");
            }
            other => panic!("expected end event: {other:?}"),
        }
        assert!(
            runtime.list_runs().is_empty(),
            "settled runs leave the table"
        );
    }

    #[tokio::test]
    async fn unknown_provider_is_loud() {
        let runtime = SubagentRuntime::new();
        let err = runtime
            .start("ghost", test_request("l", "p"))
            .await
            .unwrap_err();
        assert!(matches!(err, SubagentError::UnknownProvider(_)));
    }

    /// Fail-loud capability negotiation: an option the provider does not
    /// support rejects the start instead of being accepted-then-ignored.
    #[tokio::test]
    async fn unsupported_capability_rejects_the_start() {
        let runtime = SubagentRuntime::new();
        let caps = Capabilities {
            agent_options: false,
            ..full_caps()
        };
        let _guard = runtime.register_provider(Arc::new(ScriptedProvider::new(caps)));
        let mut request = test_request("l", "p");
        request.agent_options = Some(crate::ext::subagent::types::AgentOptions::default());
        let err = runtime.start("scripted", request).await.unwrap_err();
        assert!(
            err.to_string().contains("agent_options"),
            "the error names the capability: {err}"
        );
    }

    #[tokio::test]
    async fn depth_budget_exhaustion_rejects_the_start() {
        let runtime = SubagentRuntime::new();
        let _guard = runtime.register_provider(Arc::new(ScriptedProvider::new(full_caps())));
        let mut request = test_request("l", "p");
        request.parent_depth = 2;
        request.max_depth = Some(2);
        let err = runtime.start("scripted", request).await.unwrap_err();
        assert!(err.to_string().contains("depth budget exhausted"), "{err}");
    }

    #[tokio::test]
    async fn provider_guard_removal_on_drop() {
        let runtime = SubagentRuntime::new();
        {
            let guard = runtime.register_provider(Arc::new(ScriptedProvider::new(full_caps())));
            assert_eq!(runtime.list_providers(), vec!["scripted".to_string()]);
            drop(guard);
        }
        assert!(runtime.list_providers().is_empty());
    }

    #[tokio::test]
    async fn interrupt_cancels_the_live_run() {
        let runtime = SubagentRuntime::new();
        let _guard = runtime.register_provider(Arc::new(ScriptedProvider::new(full_caps())));
        let run = runtime
            .start("scripted", test_request("l", "p"))
            .await
            .unwrap();
        assert!(runtime.is_live(&run.id));
        assert!(runtime.interrupt(&run.id));
        assert!(!runtime.interrupt("sub-999"), "unknown id");
    }

    #[tokio::test]
    async fn continuable_rejects_on_a_provider_without_the_capability() {
        let runtime = SubagentRuntime::new();
        let _guard = runtime.register_provider(Arc::new(ScriptedProvider::new(full_caps())));
        let err = runtime
            .start_continuable("scripted", test_request("l", "p"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("continuable"), "{err}");
    }

    /// Delegation-name registry: names reserved here are exactly the set the
    /// spawn provider strips from child snapshots.
    #[test]
    fn delegation_names_round_trip() {
        let runtime = SubagentRuntime::new();
        runtime.register_delegation_tool("Explore");
        runtime.register_delegation_tool("ListAgents");
        let names = runtime.delegation_tool_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains("Explore") && names.contains("ListAgents"));
    }
}
