//! Stateful title lifecycle for `Thread`.
//!
//! Holds the LLM-generated title, the in-flight lock, the re-eval cadence
//! counter, and the user rename override. Pure request construction / streaming
//! lives in [`crate::title`]; this module owns the mutable state and the
//! detached spawn that drives a title turn. `Thread` passes runtime context
//! (depth, model, messages) into [`TitleState::maybe_generate`], so this struct
//! holds no reference back to the `Thread` or its message list outside that call.

use gpui::{App, AsyncApp, Context};

use crate::language_model::{AnyLanguageModel, LanguageModelRequest, Role};
use crate::message::Message;
use crate::thread::{Thread, message_has_text};

/// Title state owned by `Thread`. Display precedence is rename > LLM title >
/// mechanical summary (the summary fallback lives on `Thread`, which owns the
/// message list).
#[derive(Default)]
pub struct TitleState {
    title: Option<String>,
    in_flight: bool,
    last_eval_user_count: Option<usize>,
    override_: Option<String>,
}

impl TitleState {
    /// Seed from a persisted `ThreadRecord` on restore. `last_eval_user_count`
    /// is derived from whether a title already exists, so a reloaded thread
    /// continues the cadence without re-evaluating immediately.
    pub fn restore(
        title: Option<String>,
        override_: Option<String>,
        last_eval_user_count: Option<usize>,
    ) -> Self {
        Self {
            title,
            in_flight: false,
            last_eval_user_count,
            override_,
        }
    }

    /// The LLM title if non-empty.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref().filter(|s| !s.trim().is_empty())
    }

    /// Snapshot of the LLM title for persistence.
    pub fn snapshot_title(&self) -> Option<String> {
        self.title.clone()
    }

    /// Snapshot of the user rename for persistence.
    pub fn snapshot_override(&self) -> Option<String> {
        self.override_.clone()
    }

    fn override_title(&self) -> Option<&str> {
        self.override_.as_deref().filter(|s| !s.trim().is_empty())
    }

    /// Display precedence: rename > LLM title. `None` when both empty so the
    /// caller (`Thread::display_title`) falls back to the mechanical summary.
    pub fn display(&self) -> Option<&str> {
        self.override_title().or(self.title())
    }

    /// Maybe kick off an LLM title stream after a turn. Two modes:
    /// - **first title** (`title` still `None`): build a request from the first
    ///   user message + latest assistant reply and stream a concise title.
    /// - **topic-shift re-eval** (`title` already set): on a cadence (first 3
    ///   user turns, then every 5th), ask the model to emit a new title or the
    ///   literal `UNCHANGED` sentinel.
    ///
    /// Bails out for sub-agents (`depth != 0`), when a title stream is already
    /// in flight, when there is no assistant text yet, when the current
    /// user-count was already evaluated, or when the cadence says skip. The
    /// stream runs in a detached task; on success it stores the title (unless
    /// the model said `UNCHANGED`) and persists with `touch=true` so the sidebar
    /// refreshes.
    pub fn maybe_generate(
        &mut self,
        depth: u32,
        model: Option<&AnyLanguageModel>,
        messages: &[Message],
        cx: &mut Context<Thread>,
    ) {
        if depth != 0 || self.in_flight {
            return;
        }
        let Some(model) = model.cloned() else {
            return;
        };
        if !messages
            .iter()
            .any(|m| m.role == Role::Assistant && message_has_text(m))
        {
            return;
        }
        let user_count = messages
            .iter()
            .filter(|m| m.role == Role::User && message_has_text(m))
            .count();
        if self.last_eval_user_count == Some(user_count) {
            return;
        }
        if self.title.is_some() && !crate::title::should_retitle(user_count) {
            return;
        }
        let is_first = self.title.is_none();
        let request: LanguageModelRequest = if is_first {
            crate::title::build_title_request(messages)
        } else {
            crate::title::build_topic_shift_request(self.title.as_deref().unwrap_or(""), messages)
        };
        self.in_flight = true;
        self.last_eval_user_count = Some(user_count);
        let entity = cx.entity();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = crate::title::stream_thread_title(&model, request, cx).await;
            // Surface every title-generation outcome. A failure here used to be
            // swallowed by the `if let Ok` below, so an empty title (the symptom:
            // the Bot row falls back to the mechanical summary) left no trace.
            // `warn` on error / empty-text so the default log level shows it;
            // `debug` for the benign unchanged / success paths.
            match &result {
                Ok(title) if title.is_empty() => {
                    tracing::warn!("title generation produced no usable text")
                }
                Ok(title) if crate::title::is_unchanged(title) => {
                    tracing::debug!("title unchanged by model");
                }
                Ok(title) => {
                    tracing::debug!(title = %title, "title updated");
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:?}"), "title generation stream failed")
                }
            }
            let mut changed = false;
            this.update(cx, |t, cx| {
                t.title_state.in_flight = false;
                if let Ok(title) = result
                    && !title.is_empty()
                    && !crate::title::is_unchanged(&title)
                {
                    t.title_state.title = Some(title);
                    changed = true;
                }
                cx.notify();
            })
            .ok();
            if changed {
                // Persist outside the thread's write lease. `save_thread` reads
                // the entity snapshot (`thread.read(cx)`); doing that inside
                // `this.update` would re-lease the same entity and trip gpui's
                // double-lease panic — the SIGABRT from thread `4543a630`.
                cx.update(|cx: &mut App| {
                    crate::thread_store::save_thread(entity, true, cx);
                });
            }
        })
        .detach();
    }
}
