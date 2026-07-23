//! Braille-dot spinner: cycles through `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` to
//! indicate in-progress state, replacing the old rotating-circle `Spinner`.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Hsla, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{Sizable, Size};

/// Braille frames in animation order (classic 10-frame cycle).
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A text-based braille-dot spinner.
#[derive(IntoElement)]
pub struct BrailleSpinner {
    size: Size,
    speed: Duration,
    color: Option<Hsla>,
}

impl Default for BrailleSpinner {
    fn default() -> Self {
        Self::new()
    }
}

impl BrailleSpinner {
    pub fn new() -> Self {
        Self {
            size: Size::Medium,
            speed: Duration::from_millis(800),
            color: None,
        }
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl Sizable for BrailleSpinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for BrailleSpinner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let el = div();
        let el = match self.size {
            Size::XSmall => el.text_xs(),
            Size::Small => el.text_sm(),
            _ => el.text_base(),
        };
        let frame_count = FRAMES.len() as f32;

        el.when_some(self.color, |this, color| this.text_color(color))
            .with_animation(
                "braille-spinner",
                Animation::new(self.speed).repeat(),
                move |this, delta| {
                    let frame = (delta * frame_count) as usize % FRAMES.len();
                    this.child(SharedString::from(FRAMES[frame]))
                },
            )
            .into_element()
    }
}
