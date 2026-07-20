//! Asset source overlaying manox-specific SVG icons on top of
//! `gpui-component-assets`.
//!
//! `gpui-component-assets` ships the icon set `IconName` resolves to, but it
//! cannot carry manox's own brand icons (the Manox / Claude / Codex / GitHub Copilot
//! marks used in the sidebar and the new-session menu). `ExtrasAssetSource`
//! layers those on top: a `rust-embed` lookup of `assets/icons/**` wins, then
//! it falls through to `gpui-component-assets` for everything else. The manox
//! bin registers it via `with_assets`, so any `gpui::svg().path("icons/…")`
//! call site resolves through here.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component_assets::Assets as ComponentAssets;
use rust_embed::RustEmbed;

/// Embedded manox-local SVG assets (brand icons not in gpui-component).
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
struct LocalAssets;

/// Hybrid asset source: manox-local SVGs first, then `gpui-component-assets`
/// for the shared icon set. Mirrors the gpui-manos-assets pattern.
pub struct ExtrasAssetSource;

impl ExtrasAssetSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ExtrasAssetSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetSource for ExtrasAssetSource {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = LocalAssets::get(path) {
            return Ok(Some(file.data));
        }
        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let path = path.trim_matches('/');
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };

        let mut children: Vec<SharedString> = Vec::new();

        for asset_path in LocalAssets::iter() {
            if !asset_path.starts_with(&prefix) {
                continue;
            }
            let rest = &asset_path[prefix.len()..];
            let name = rest.split('/').next().unwrap_or(rest);
            if !children.iter().any(|item| item.as_ref() == name) {
                children.push(SharedString::from(name.to_string()));
            }
        }

        if let Ok(component_children) = ComponentAssets.list(path) {
            for child in component_children {
                if !children.iter().any(|item| item.as_ref() == child.as_ref()) {
                    children.push(child);
                }
            }
        }

        Ok(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_every_new_session_brand_icon() {
        for path in [
            "icons/manox.svg",
            "icons/claude.svg",
            "icons/codex.svg",
            "icons/githubcopilot.svg",
        ] {
            assert!(LocalAssets::get(path).is_some(), "missing {path}");
        }
    }
}
