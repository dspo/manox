//! Standalone terminal example: running this binary yields the zterm terminal.
//!
//! ```sh
//! cargo run -p zterm-ui --example zterm
//! ```

use gpui::{App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use util::ResultExt;
use zterm_core::ToggleViMode;
use zterm_ui::TerminalView;

fn main() {
    application().run(|cx: &mut App| {
        cx.activate(true);
        cx.bind_keys([KeyBinding::new("ctrl-shift-v", ToggleViMode, None)]);
        let bounds = Bounds::centered(None, size(px(1000.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("zterm");
                let view = cx.new(TerminalView::new);
                window.focus(&view.read(cx).focus_handle(), cx);
                view
            },
        )
        .log_err();
    });
}
