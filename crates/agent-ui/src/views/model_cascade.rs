//! Provider→model cascade shared by every external-agent launch surface.
//!
//! Models are drawn from the shared pi provider registry, filtered by the
//! agent id (registration metadata `agents`, empty = visible to all); they
//! are grouped by provider display name, each provider a nested submenu. A
//! config model registered through several wire apis appears once per wire
//! endpoint (exact duplicates collapse), each row tagged with its wire api
//! like the composer model menu. Picking a model invokes `on_pick` with
//! (provider, model id, wire) — the emitted model id is the raw cx config
//! key (`metadata["config_id"]`), which cx matches verbatim; the wire key
//! pins the endpoint variant at launch resolution.

use std::collections::HashSet;

use crate::i18n;
use gpui::{App, Context, Window, prelude::*};
use gpui_component::{
    Sizable as _, h_flex,
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
};

pub(crate) fn build_model_cascade(
    menu: PopupMenu,
    agent_id: &'static str,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    on_pick: impl Fn(String, String, Option<String>, &mut Window, &mut App) + Clone + 'static,
) -> PopupMenu {
    /// One cascade entry: the raw cx config key, its display name, and the
    /// wire endpoint variant (row tag + cx wire key for the launch pin).
    struct Entry {
        config_id: String,
        display: String,
        tag: (TagVariant, &'static str),
        wire: Option<String>,
    }
    let mut providers: Vec<(String, Vec<Entry>)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for m in manox_agent::pi_providers::global().models() {
        // Missing metadata = non-cx registration (visible); otherwise the
        // effective agent list must contain the cascade's agent (parity
        // with the retired manox `visible_agents` filter).
        let visible = m
            .metadata
            .get("agents")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .any(|a| a.as_str().is_some_and(|a| a == agent_id))
            })
            .unwrap_or(true);
        if !visible {
            continue;
        }
        let prov = manox_agent::pi_providers::display_provider_name(&m);
        let config_id = manox_agent::pi_providers::config_id(&m);
        // Identity is the registration name (unique per wire endpoint), so
        // wire variants of one provider stay separate; only exact
        // duplicates collapse (parity with the composer model menu).
        if !seen.insert((m.provider.clone(), config_id.clone())) {
            continue;
        }
        let entry = Entry {
            config_id,
            display: manox_agent::pi_providers::display_name(&m),
            tag: crate::Workspace::pi_wire_tag_variant(&m.api),
            wire: manox_agent::pi_providers::wire_key(&m).map(str::to_string),
        };
        // Lookup-based grouping (not adjacency): the registry is sorted by
        // registration name, so equal display names must still merge.
        match providers.iter_mut().find(|(name, _)| *name == prov) {
            Some((_, entries)) => entries.push(entry),
            None => providers.push((prov, vec![entry])),
        }
    }

    let mut menu = menu;
    if providers.is_empty() {
        menu = menu.label(i18n::t("external-wizard-no-model"));
        return menu;
    }
    for (prov_name, models) in providers {
        let prov_for_items = prov_name.clone();
        let on_pick = on_pick.clone();
        menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
            let mut submenu = submenu;
            for m in &models {
                let model_id = m.config_id.clone();
                let model_name = m.display.clone();
                let (variant, label) = m.tag;
                let wire = m.wire.clone();
                let prov = prov_for_items.clone();
                let on_pick = on_pick.clone();
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
                            .into_any_element()
                    })
                    .on_click(move |_e, window, cx: &mut App| {
                        on_pick(prov.clone(), model_id.clone(), wire.clone(), window, cx);
                    }),
                );
            }
            submenu
        });
    }
    menu
}
