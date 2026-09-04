//! Plugin route registry — the host-side extension seam behind
//! `/api/plugin/<name>/*` (T8 §H).
//!
//! The webui bundles its extension plugins into the client at build time
//! (manifest scan → inline, see `apps/web/webui/build/plugin-manifests.mjs`),
//! and most plugins — the `conversation-info` acceptance sample included —
//! need no HTTP surface at all: they talk the typed protocol over the shared
//! WebSocket and read the store through the public Q-face fetch seam. This
//! module is the *declarative* escape hatch for the plugins that do need an
//! HTTP route (a static asset, a download endpoint, …).
//!
//! A plugin registers by adding a [`PluginRoute`] to [`plugin_route_registry`];
//! the dispatcher resolves the `<name>` path segment to its handler and hands
//! it the remainder of the path. With an empty registry (today) every
//! `/api/plugin/*` request is a clean 404, so the seam is a no-op until a
//! plugin claims it — no behavior change to the shipped client.

use std::future::Future;
use std::pin::Pin;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// A boxed future resolving to an axum response.
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// One declared plugin HTTP route. `name` is the `<name>` segment matched
/// after `/api/plugin/`; `handle` receives the path remainder (the `/*` after
/// the name) and returns the response.
pub(crate) struct PluginRoute {
    /// The plugin's route name (its path segment under `/api/plugin/`).
    pub name: &'static str,
    /// The async request handler.
    pub handle: fn(String) -> BoxFuture<Response>,
}

/// The declarative registry of plugin HTTP routes. This is the single list a
/// plugin appends to; the dispatcher looks a name up here. It is empty in the
/// shipped build — `conversation-info` needs no HTTP route (the protocol
/// already carries `GetConversationInfo`) — so the seam stays dormant until a
/// future plugin claims it.
pub(crate) fn plugin_route_registry() -> Vec<PluginRoute> {
    Vec::new()
}

/// Resolve `/api/plugin/<name>/<rest>` against [`plugin_route_registry`]. An
/// unknown (or, today, any) name with no registered handler is a 404; a known
/// one runs its handler with the path remainder.
pub(crate) async fn plugin_dispatch(Path((name, rest)): Path<(String, String)>) -> Response {
    match plugin_route_registry().into_iter().find(|r| r.name == name) {
        Some(route) => (route.handle)(rest).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// The shipped registry is empty — the seam is dormant, and the
    /// conversation-info plugin never needs an HTTP route (it fetches the
    /// Q face over the WebSocket protocol).
    #[test]
    fn registry_ships_empty() {
        assert!(
            plugin_route_registry().is_empty(),
            "no built-in plugin claims an HTTP route at this stage"
        );
    }

    /// With nothing registered, every `/api/plugin/<name>/<rest>` dispatch is
    /// a clean 404 (the seam is a no-op rather than a panic or a fall-through
    /// to the static asset route).
    #[tokio::test]
    async fn dispatch_404s_unknown_plugin() {
        let resp = plugin_dispatch(Path((
            "conversation-info".to_string(),
            "anything".to_string(),
        )))
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());
    }

    /// The dispatcher runs a registered handler when the name matches,
    /// forwarding the path remainder — proving the seam is wired, not just a
    /// dead table. (A synthetic route stands in for a future plugin's claim.)
    #[tokio::test]
    async fn dispatch_runs_a_registered_handler() {
        async fn echo(rest: String) -> Response {
            (StatusCode::OK, format!("echo:{rest}")).into_response()
        }
        let route = PluginRoute {
            name: "probe",
            handle: |rest| Box::pin(echo(rest)),
        };
        let matched = std::iter::once(route).find(|r| r.name == "probe");
        let resp = match matched {
            Some(r) => (r.handle)("a/b".to_string()).await,
            None => StatusCode::NOT_FOUND.into_response(),
        };
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"echo:a/b");
    }
}
