// In-process extensions for the harness core.
//
// The kernel crate defines the seams (`BashOperations`,
// `BackgroundTaskRegistry`); this crate implements them stack-internally —
// no dynamic loading, no out-of-process runtime — and assembles the
// product-level bash tool on top.

pub mod bash;
pub mod model_ref;
pub mod monitor;
pub mod path_selector;
pub mod prompt;
pub mod provider;
pub mod read;
pub mod sandbox;
pub mod session_meta;
pub mod session_stream;
pub mod steer_bus;
pub mod subagent;
pub mod tasks;

pub use bash::background::{BackgroundRegistry, BashOutputTool, TaskStopTool};
pub use bash::persistent::PersistentShellOperations;
pub use monitor::MonitorTool;
pub use subagent::SubagentRuntime;
pub use tasks::{
    Settlement, SettlementCause, SettlementKind, TaskFamily, TaskLifecycle, TaskObserver,
};

/// Process-global ordinal for background task ids. Every registry
/// (`BackgroundRegistry`, `WsMonitorRegistry`) draws from this one counter so
/// ids stay unique across concurrently-live sessions in one process; the
/// host's task registry is process-global and keys on these ids.
pub(crate) fn next_task_ordinal() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use crate::ext::bash::background::BackgroundRegistry;
    use crate::ext::monitor::WsMonitorRegistry;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    /// Two ids sharing an ordinal suffix would alias one entry in the host's
    /// process-global task registry. This pins the #829 fix: the ordinal is
    /// one process-wide counter, not a per-registry one — two live
    /// `BackgroundRegistry` instances (one per session) must never mint the
    /// same id, and the `mon_`/`bg_`/`ws_` spaces must not overlap.
    #[tokio::test]
    async fn ordinals_stay_unique_across_registries_and_prefixes() {
        let spawn_ids = |registry: &BackgroundRegistry| {
            let mut ids = Vec::new();
            for _ in 0..2 {
                ids.push(
                    registry
                        .spawn_with_line_events(
                            "true",
                            Path::new("/tmp"),
                            Box::new(|_, _| {}),
                            Box::new(|_, _| {}),
                        )
                        .expect("mon_ spawn")
                        .0,
                );
                ids.push(
                    registry
                        .spawn_escalated_with_line_events(
                            "true",
                            Path::new("/tmp"),
                            Box::new(|_, _| {}),
                            Box::new(|_, _| {}),
                        )
                        .expect("bg_ spawn")
                        .0,
                );
            }
            ids
        };
        let first = spawn_ids(&BackgroundRegistry::new());
        let second = spawn_ids(&BackgroundRegistry::new());
        let ws = WsMonitorRegistry::new();
        let monitors: Vec<String> = (0..2)
            .map(|_| {
                ws.register("ws://127.0.0.1:9/unused".into(), CancellationToken::new())
                    .0
            })
            .collect();

        let all: Vec<&String> = first.iter().chain(&second).chain(&monitors).collect();
        let ordinals: Vec<u64> = all
            .iter()
            .map(|id| {
                id.split_once('_')
                    .expect("id carries its prefix")
                    .1
                    .parse()
                    .expect("ordinal suffix")
            })
            .collect();
        let mut unique = ordinals.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            ordinals.len(),
            "two registries minted the same ordinal: {all:?}"
        );

        let in_prefix = |prefix: &str| {
            let ids: Vec<&String> = all
                .iter()
                .filter(|id| id.starts_with(prefix))
                .copied()
                .collect();
            assert!(!ids.is_empty(), "no {prefix} ids were minted: {all:?}");
            let mut sorted: Vec<&String> = ids.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), ids.len(), "duplicate {prefix} ids: {ids:?}");
        };
        for prefix in ["mon_", "bg_", "ws_"] {
            in_prefix(prefix);
        }
    }
}
