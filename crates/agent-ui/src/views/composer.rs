//! Composer rendering for the `Workspace`: the inline input area, its chip
//! row (cwd label + model selector), and the cascading model popup menu.
//!
//! These are `impl Workspace` methods that live apart from `workspace.rs` so
//! that file can stay a thin assembly layer. Field/method access crosses the
//! module boundary via `pub(crate)`.

use agent::provider::config::WireApi;
use agent::provider::registry;
use gpui::{AnyElement, Context, DismissEvent, Entity, Window, prelude::*, px};
use gpui_component::{
    Disableable as _, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
};

use crate::Workspace;
use crate::views::CONTENT_MAX_W;

impl Workspace {
    fn render_model_selector(&mut self, _theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let label = self.model_label(cx);
        let open = self.model_open;

        let trigger = Button::new("model-trigger")
            .ghost()
            .small()
            .label(label)
            .icon(if open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .on_click(cx.listener(|this, _, window, cx| {
                if this.model_open {
                    this.model_open = false;
                    this.model_menu = None;
                    this.model_menu_sub = None;
                } else {
                    this.model_open = true;
                    let workspace = cx.entity();
                    let menu = PopupMenu::build(window, cx, |menu, window, cx| {
                        Self::build_model_popup_menu(menu, workspace, window, cx)
                    });
                    let sub = cx.subscribe(
                        &menu,
                        |this: &mut Workspace,
                         _menu: Entity<PopupMenu>,
                         _: &DismissEvent,
                         cx: &mut Context<Workspace>| {
                            this.model_open = false;
                            this.model_menu = None;
                            this.model_menu_sub = None;
                            cx.notify();
                        },
                    );
                    this.model_menu = Some(menu);
                    this.model_menu_sub = Some(sub);
                }
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .model_menu
            .clone()
            .expect("model_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                // PopupMenu has its own bg/border/shadow and on_mouse_down_out.
                // `.occlude()` renders the dropdown above all non-occluded elements
                // (footer borders, message list, etc.).
                gpui::div()
                    .id("model-dropdown")
                    .absolute()
                    .bottom_full()
                    .right_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    /// WireApi → Tag variant + label mapping for the model menu.
    fn wire_tag_variant(wire: WireApi) -> (TagVariant, &'static str) {
        match wire {
            WireApi::Anthropic => (TagVariant::Primary, "Anthropic"),
            WireApi::Responses => (TagVariant::Info, "Responses"),
            WireApi::Completions => (TagVariant::Warning, "Completions"),
            WireApi::Unavailable => (TagVariant::Secondary, "N/A"),
        }
    }

    /// Cascading model menu grouped by provider; each model row shows a wire-api Tag.
    fn build_model_popup_menu(
        menu: PopupMenu,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let mut providers: Vec<(String, Vec<agent::language_model::AnyLanguageModel>)> = Vec::new();
        for m in registry::global().models() {
            let prov = m.provider_name();
            if let Some(last) = providers.last_mut()
                && last.0 == prov
            {
                last.1.push(m.clone());
            } else {
                providers.push((prov, vec![m.clone()]));
            }
        }

        let mut menu = menu;
        if providers.is_empty() {
            menu = menu.item(PopupMenuItem::Label("No models configured".into()));
        }
        for (prov_name, models) in providers {
            let ws = workspace.clone();
            menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
                let mut submenu = submenu;
                for m in &models {
                    let model_id = m.id();
                    let model_name = m.name().to_string();
                    let wire = m.wire_api();
                    let (variant, label) = Self::wire_tag_variant(wire);
                    let ws = ws.clone();
                    submenu = submenu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Tag::new()
                                        .with_variant(variant)
                                        .outline()
                                        .small()
                                        .child(label),
                                )
                                .child(model_name.clone())
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            ws.update(cx, |this, cx| {
                                if let Some(m) = registry::global().get_model(model_id.as_ref()) {
                                    this.thread.update(cx, |t, cx| t.set_model(m, cx));
                                }
                            });
                        }),
                    );
                }
                submenu
            });
        }
        menu
    }

    pub(crate) fn render_composer(
        &self,
        running: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_end()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                Button::new("composer-plus")
                    .ghost()
                    .icon(IconName::Plus)
                    .disabled(true),
            )
            .child(gpui::div().flex_1().child(Input::new(&self.input_state)))
            .child(self.render_send_button(running, cx))
            .into_any_element()
    }

    /// Circular icon-only send/stop button.
    ///
    /// Reuses `Button` for built-in focus ring, keyboard activation, and disabled handling;
    /// `.rounded(px(16.))` renders the button as a 32px disc.
    fn render_send_button(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        Button::new("send-btn")
            .icon(if running {
                IconName::Pause
            } else {
                IconName::ArrowUp
            })
            .when(running, |b| b.danger())
            .when(!running, |b| b.primary())
            .rounded(px(16.))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.thread.read(cx).is_running() {
                    this.cancel_turn(cx);
                } else {
                    this.submit_input(window, cx);
                }
            }))
            .into_any_element()
    }

    /// Chip row: a plain `cwd` label on the left, model selector on the right.
    pub(crate) fn render_chip_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Self-contained centering (not `centered()` helper) so the absolute-positioned
        // model dropdown is not clipped by any `max_w` ancestor.
        h_flex()
            .w_full()
            .justify_center()
            .child(
                h_flex()
                    .w_full()
                    .max_w(px(CONTENT_MAX_W))
                    .items_center()
                    .child(self.render_cwd_chip(theme))
                    .child(gpui::div().flex_1())
                    .child(
                        gpui::div()
                            .flex_shrink_0()
                            .child(self.render_model_selector(theme, cx)),
                    ),
            )
            .into_any_element()
    }

    /// Static label of the current working directory's basename.
    ///
    /// Rendered as a plain `div` (not `Button`) until a directory-switcher popover exists; an
    /// unclickable `Button` would invite clicks that do nothing.
    fn render_cwd_chip(&self, theme: &Theme) -> AnyElement {
        let name = self
            .cwd
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| *s != ".")
            .unwrap_or("project");
        gpui::div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(name.to_string())
            .into_any_element()
    }
}
