//! VS Code launch actions for the macOS `工具 → VS Code` menu cascade. The
//! cascade itself lives in the bin (`build_tools_menu`); the App-level menu
//! handlers route here through `Workspace::launch_vscode_app` /
//! `Workspace::launch_vscode_plain`.

/// Launch VS Code with the selected provider + model injected as Claude Code
/// BYOK env (cx's vscode_app injection path; restarts a running VS Code after
/// user confirmation). Dispatched from native menu items, each carrying its
/// own payload instance; never bound in a keymap, so no JSON build support.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = agent_ui, no_json)]
pub struct LaunchVSCode {
    pub provider: String,
    pub model: String,
}

/// Launch VS Code without BYOK injection (equivalent to a normal Dock open).
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = agent_ui, no_json)]
pub struct LaunchVSCodePlain;
