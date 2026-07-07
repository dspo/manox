//! Test-only helpers: a replay mock model and Workspace scaffolding for the
//! two-concurrent-threads repro. Mirrors agent's private `ReplayMockModel`
//! (`thread.rs`) and live-test setup so `agent-ui` tests can drive turns
//! offline without touching a real provider.

#![cfg(test)]

use std::sync::Arc;

use agent::db::ThreadsDatabase;
use agent::language_model::{
    AnyLanguageModel, LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
};
use agent::provider::WireApi;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use gpui::{Entity, TestAppContext};

use crate::workspace::Workspace;

/// Mock `LanguageModel` that replays a fixed event sequence. `Arc`-shared so
/// two concurrent workspaces can stream from the same model instance.
pub(crate) struct ReplayModel {
    id: String,
    events: Arc<Vec<LanguageModelCompletionEvent>>,
}

impl ReplayModel {
    pub(crate) fn build(id: &str, events: Vec<LanguageModelCompletionEvent>) -> AnyLanguageModel {
        Arc::new(Self {
            id: id.into(),
            events: Arc::new(events),
        })
    }
}

impl LanguageModel for ReplayModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.id.clone()
    }
    fn provider_id(&self) -> String {
        "test".into()
    }
    fn provider_name(&self) -> String {
        "test".into()
    }
    fn wire_api(&self) -> WireApi {
        WireApi::Anthropic
    }
    fn max_token_count(&self) -> u64 {
        4096
    }
    fn stream_completion(
        &self,
        _request: LanguageModelRequest,
        _cx: &gpui::AsyncApp,
    ) -> BoxFuture<
        'static,
        anyhow::Result<BoxStream<'static, anyhow::Result<LanguageModelCompletionEvent>>>,
    > {
        let events = self.events.clone();
        Box::pin(async move {
            let events: Vec<_> = events.iter().cloned().map(Ok).collect();
            Ok(futures::stream::iter(events).boxed())
        })
    }
}

/// Releases the test `ThreadStore` entity on drop so gpui's leaked-handle
/// check at teardown passes. Hold alive for the test's duration.
pub(crate) struct StoreGuard;
impl Drop for StoreGuard {
    fn drop(&mut self) {
        agent::thread_store::drop_for_test();
    }
}

/// Initialize the full agent stack (runtime, i18n, provider registry, agent
/// defs) and override the `ThreadStore` with an in-memory db. Reads the real
/// provider config at `~/.config/cx/cx.providers.config.yaml`, so tests that
/// need a model require that file present — they are `#[ignore]`-gated.
pub(crate) fn setup(cx: &mut TestAppContext) -> StoreGuard {
    cx.update(agent::init);
    let db =
        Arc::new(ThreadsDatabase::open(std::path::Path::new(":memory:")).expect("open mem db"));
    cx.update(|cx| agent::thread_store::init_for_test(db, cx));
    StoreGuard
}

/// Open a Workspace in a test window and pin the given model onto its thread.
pub(crate) fn build_workspace(
    cx: &mut TestAppContext,
    model: AnyLanguageModel,
) -> Entity<Workspace> {
    let (ws, _vctx) = cx.add_window_view(Workspace::new);
    cx.update(|cx| {
        ws.update(cx, |ws, cx| {
            ws.thread.update(cx, |t, cx| t.set_model(model, cx));
        });
    });
    ws
}
