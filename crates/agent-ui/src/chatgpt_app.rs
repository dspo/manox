//! ChatGPT.app launch action for the macOS `工具 → ChatGPT.app` menu cascade.
//! The cascade itself lives in the bin (`build_app_menus`); the App-level menu
//! handler routes here through `Workspace::launch_chatgpt_app`.

/// Launch ChatGPT.app with the selected provider + default model via cx's
/// injection path. Dispatched from native menu items, each carrying its own
/// payload instance; never bound in a keymap, so no JSON build support.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = agent_ui, no_json)]
pub struct LaunchChatGptApp {
    pub provider: String,
    pub model: String,
}
