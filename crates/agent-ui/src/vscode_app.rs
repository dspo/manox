//! VS Code launch actions for the macOS `工具 → VS Code` menu entry. The menu
//! itself lives in the bin (`build_tools_menu`); the App-level menu handler
//! routes here through `Workspace::launch_vscode_app`. Injection targets are
//! not chosen at launch time — they come from the persisted `vscode_app:`
//! settings (Settings → 外部工具 → Visual Studio Code.app).

/// Launch VS Code with injections resolved from the persisted `vscode_app:`
/// settings (Claude Code Extension ANTHROPIC_* env and/or Codex Extension
/// CODEX_HOME + config.toml; restarts a running VS Code after user
/// confirmation). Dispatched from a native menu item; never bound in a
/// keymap, so no JSON build support.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = agent_ui, no_json)]
pub struct LaunchVSCode;
