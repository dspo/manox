#![allow(dead_code)] // each test binary links the shared helpers it uses; the rest is unused per-binary

//! Shared harness for the interaction-card regression binaries. Each test
//! binary holds exactly ONE `#[gpui::test]`: the tests initialize
//! process-global singletons (`manox_agent::runtime`, `pi_providers`,
//! `thread_store`) whose global store entity is app-affine, so two tests in
//! one process would overwrite each other's store under the parallel test
//! harness (see the `workspace_overlap` binary for the same constraint).

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use agent_ui::Workspace;
use gpui::{AppContext as _, Entity, TestAppContext, px, size};
use gpui_component::Theme;
use manox_agent::Thread;
use manox_agent::ThreadEvent;
use manox_agent::db::ThreadSummary;
use manox_agent::language_model::{LanguageModelToolUse, MessageContent, TokenUsage};
use manox_agent::message::Message;
use manox_agent::thread_engine::{BackendNotice, ThreadEngine};
use pi::types::{ContentBlock, Model as PiModel};

/// Minimal backend for the workspace tests: no actor, fixed history. The
/// thread facade drives everything else through emitted `ThreadEvent`s.
pub struct FakeEngine {
    history: std::sync::Mutex<Vec<Message>>,
}

impl ThreadEngine for FakeEngine {
    fn is_running(&self) -> bool {
        false
    }
    fn history(&self) -> Vec<manox_agent::db::HistoryEntry> {
        self.history
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .map(manox_agent::db::HistoryEntry::Message)
            .collect()
    }
    fn request_token_usage(&self) -> std::collections::HashMap<String, TokenUsage> {
        std::collections::HashMap::new()
    }
    fn model(&self) -> Option<PiModel> {
        None
    }
    fn run(&self, _prompt: String, _images: Vec<ContentBlock>) {}
    fn steer(&self, _text: String, _images: Vec<ContentBlock>) -> String {
        String::new()
    }
    fn cancel_steer(&self, _id: &str) -> bool {
        false
    }
    fn abort(&self) {}
    fn set_model(&self, _model: PiModel) {}
    fn set_thinking_level(&self, _level: Option<String>) {}
    fn open_session(&self, _path: PathBuf) {}
    fn new_session(&self, _cwd: PathBuf, _project: Option<PathBuf>) {}
    fn active_session_path(&self) -> Option<PathBuf> {
        None
    }
    fn session_list(&self) -> Vec<ThreadSummary> {
        Vec::new()
    }
}

fn register_lilex(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!(
                    "../../../manox/assets/fonts/lilex/Lilex-Light.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../../manox/assets/fonts/lilex/Lilex-Medium.ttf"
                )),
            ])
            .expect("Lilex fonts");
        let theme = Theme::global_mut(cx);
        theme.mono_font_family = "Lilex".into();
    });
}

pub fn init_harness(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    register_lilex(cx);
    cx.update(|_cx| {
        manox_agent::runtime::init();
        manox_agent::provider_glue::init();
        manox_agent::thread_store::init();
    });
}

/// Open the production workspace shell and return the window + workspace.
pub fn open_workspace(
    cx: &mut TestAppContext,
) -> (gpui::WindowHandle<gpui_component::Root>, Entity<Workspace>) {
    let cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = cell.clone();
    let window = cx.open_window(size(px(1_120.), px(780.)), move |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(window, cx));
        *capture.borrow_mut() = Some(workspace.clone());
        gpui_component::Root::new(workspace, window, cx)
    });
    cx.run_until_parked();
    (window, cell.borrow().clone().expect("workspace captured"))
}

/// A thread facade with a fake engine: `Thread::landing` defers engine
/// creation, so the test swaps in the fake before anything runs. Returns the
/// gpui-free `ThreadHandle`; the workspace binds its own `ClientStoreHandle`
/// on `attach_thread`, and tests drive it through `emit`.
pub fn fake_thread(
    cx: &mut TestAppContext,
    history: Vec<Message>,
) -> manox_agent::thread::ThreadHandle {
    let (_, events) = tokio::sync::mpsc::unbounded_channel::<BackendNotice>();
    cx.update(|_cx| {
        let thread = Thread::landing(PathBuf::from("/tmp"));
        thread.with_mut(|t| {
            t.set_engine_for_test(
                Arc::new(FakeEngine {
                    history: std::sync::Mutex::new(history),
                }),
                events,
            )
        });
        thread
    })
}

/// Emit a `ThreadEvent` on the store bound to `thread_id` (foreground or a
/// parked background thread), driving the workspace's subscription handler.
pub fn emit(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
    thread_id: &str,
    event: ThreadEvent,
) {
    cx.update(|cx| {
        workspace.update(cx, |ws, cx| ws.diagnostic_emit_event(thread_id, event, cx));
    });
}

/// A real plan file on disk, as `ProposePlan` leaves one.
pub fn write_plan_file() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit-plan.md");
    std::fs::write(&path, "# Audit\n\nsteps").unwrap();
    let plan_file = path.to_string_lossy().to_string();
    (dir, plan_file)
}

pub fn bash_tool_use_message(id: &str) -> Message {
    Message::assistant(vec![MessageContent::ToolUse(LanguageModelToolUse {
        id: id.to_string(),
        name: "Bash".into(),
        raw_input: r#"{"command":"ls"}"#.to_string(),
        input: serde_json::json!({"command": "ls"}),
        is_input_complete: true,
        thought_signature: None,
    })])
}
