//! Blinking caret cadence. A tiny entity toggling visibility on a 500ms timer
//! while the composer is focused; the composer observes it and repaints. After
//! each edit the composer calls `pause`, which holds the caret solid for a short
//! delay so typing doesn't freeze it mid-blink.

use std::time::Duration;

use gpui::{Context, Pixels, Task, px};

const INTERVAL: Duration = Duration::from_millis(500);
const PAUSE_DELAY: Duration = Duration::from_millis(300);

/// Caret stroke width. Integer on non-macOS to avoid sub-pixel blur.
#[cfg(not(target_os = "macos"))]
pub const CURSOR_WIDTH: Pixels = px(2.);
#[cfg(target_os = "macos")]
pub const CURSOR_WIDTH: Pixels = px(1.5);

pub struct BlinkCursor {
    visible: bool,
    paused: bool,
    epoch: usize,
    _task: Task<()>,
}

impl BlinkCursor {
    pub fn new() -> Self {
        Self {
            visible: false,
            paused: false,
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.blink(self.epoch, cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.epoch = 0;
        self.visible = false;
        cx.notify();
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if self.paused || epoch != self.epoch {
            self.visible = true;
            return;
        }
        self.visible = !self.visible;
        cx.notify();
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INTERVAL).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink(epoch, cx));
            }
        });
    }

    pub fn visible(&self) -> bool {
        self.paused || self.visible
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.paused = true;
        self.visible = true;
        cx.notify();
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.paused = false;
                    this.blink(epoch, cx);
                });
            }
        });
    }
}

impl Default for BlinkCursor {
    fn default() -> Self {
        Self::new()
    }
}
