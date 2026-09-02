//! Utility functions for the WebUI bridge.

/// The default project directory for new sessions: the most recently
/// registered project the thread store knows, falling back to `$HOME`.
pub(crate) fn resolve_cwd() -> String {
    if let Some(store) = manox_agent::thread_store::try_global() {
        let known = store.read(|s| s.known_projects().to_vec());
        if let Some(project) = known.last() {
            return project.clone();
        }
    }
    manox_agent::paths::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
}
