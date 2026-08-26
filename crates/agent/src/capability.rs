//! Frontend capability inversion.
//!
//! The kernel expresses needs it cannot fulfil on its own — driving a browser,
//! reading the clipboard, opening an external link, querying the host editor —
//! and a *frontend* registers the provider for the capabilities it owns. The
//! kernel invokes the provider; it never implements frontend logic itself. A
//! missing provider or capability fails closed with an error, never a silent
//! fallback, so a tool surfaces "capability unavailable" instead of a fake
//! success.
//!
//! This is the in-process precursor of the protocol's capability negotiation
//! (`manox_protocol::ClientHello::capabilities` + `ServerCall` routing): when
//! the AgentServer lands, the provider's implementations become the client side
//! of those `ServerCall`s and this seam is rewired without touching call sites.

use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;

use crate::thread_engine::{BrowserOp, BrowserReply};

/// Frontend-provided capabilities the kernel may invoke.
pub trait CapabilityClient: Send + Sync {
    /// Drive the frontend's browser. Fails closed when the frontend offers no
    /// browser.
    fn browser_op(&self, op: BrowserOp) -> BoxFuture<'static, Result<BrowserReply, String>>;
}

static PROVIDER: OnceLock<Arc<dyn CapabilityClient>> = OnceLock::new();

/// Register the process-wide capability provider (App startup). The first
/// registration wins; a second is ignored (single-workspace, single-process),
/// but logged rather than dropped silently so a stray re-wire is visible.
pub fn set_provider(provider: Arc<dyn CapabilityClient>) {
    if PROVIDER.set(provider).is_err() {
        tracing::warn!("capability provider already registered; ignoring re-registration");
    }
}

/// The registered capability provider, or `None` in headless contexts (no
/// frontend registered capabilities).
pub fn provider() -> Option<&'static Arc<dyn CapabilityClient>> {
    PROVIDER.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider;

    impl CapabilityClient for MockProvider {
        fn browser_op(&self, _op: BrowserOp) -> BoxFuture<'static, Result<BrowserReply, String>> {
            Box::pin(async { Err("mock unavailable".to_string()) })
        }
    }

    // The seam is object-safe and a provider answers (or fails closed) through
    // the trait object the kernel holds.
    #[test]
    fn trait_object_answers_fail_closed() {
        let caps: Arc<dyn CapabilityClient> = Arc::new(MockProvider);
        let err = futures::executor::block_on(caps.browser_op(BrowserOp::Open {
            url: "https://example.com".into(),
        }))
        .unwrap_err();
        assert_eq!(err, "mock unavailable");
    }
}
