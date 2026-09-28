//! Host errors and the single declaration table of codes leaving the gateway.
//!
//! The v2 rule survives: every production error carries a stable code (no bare
//! messages). AHP's standard codes come from `ahp-types`; the x-manox band
//! `-32080..-32099` carries the extensions this host needs and refuses to
//! silently degrade.

use ahp_types::messages::JsonRpcError;
use serde_json::{Value, json};

/// Every code the manox AHP host may emit — the single source of truth.
///
/// `codes::DECLARED` is asserted against by tests, so a new variant without a
/// table row fails the build gate rather than leaking an undeclared code.
pub mod codes {
    pub use ahp_types::errors::{ahp_error_codes as ahp, json_rpc_error_codes as rpc};

    /// The target method/channel is a declared x-manox surface we do not serve.
    pub const X_MANOX_UNSUPPORTED: i32 = -32080;
    /// A client-dispatched action was refused by the acceptance table.
    pub const X_MANOX_ACTION_REJECTED: i32 = -32081;
    /// The `resource*` plane refused the path (outside the granted roots).
    pub const X_MANOX_RESOURCE_DENIED: i32 = -32082;
    /// The runtime backend failed while serving a command.
    pub const X_MANOX_BACKEND: i32 = -32083;

    /// Name → code, for the declaration surface and its drift gate.
    pub const DECLARED: &[(&str, i32)] = &[
        ("jsonRpc/parseError", rpc::PARSE_ERROR),
        ("jsonRpc/invalidRequest", rpc::INVALID_REQUEST),
        ("jsonRpc/methodNotFound", rpc::METHOD_NOT_FOUND),
        ("jsonRpc/invalidParams", rpc::INVALID_PARAMS),
        ("jsonRpc/internalError", rpc::INTERNAL_ERROR),
        ("ahp/sessionNotFound", ahp::SESSION_NOT_FOUND),
        ("ahp/providerNotFound", ahp::PROVIDER_NOT_FOUND),
        ("ahp/sessionAlreadyExists", ahp::SESSION_ALREADY_EXISTS),
        ("ahp/turnInProgress", ahp::TURN_IN_PROGRESS),
        (
            "ahp/unsupportedProtocolVersion",
            ahp::UNSUPPORTED_PROTOCOL_VERSION,
        ),
        ("ahp/authRequired", ahp::AUTH_REQUIRED),
        ("ahp/notFound", ahp::NOT_FOUND),
        ("ahp/permissionDenied", ahp::PERMISSION_DENIED),
        ("ahp/alreadyExists", ahp::ALREADY_EXISTS),
        ("ahp/conflict", ahp::CONFLICT),
        ("xManox/unsupported", X_MANOX_UNSUPPORTED),
        ("xManox/actionRejected", X_MANOX_ACTION_REJECTED),
        ("xManox/resourceDenied", X_MANOX_RESOURCE_DENIED),
        ("xManox/backend", X_MANOX_BACKEND),
    ];
}

/// One host-side failure, always carrying a stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The requested session URI is not known to the backend.
    SessionNotFound(String),
    /// `createSession` targeted a URI that already has a live session.
    SessionAlreadyExists(String),
    /// The client asked for a provider/model the backend does not serve.
    ProviderNotFound(String),
    /// A write arrived while a turn is running and the backend cannot queue it.
    TurnInProgress(String),
    /// No offered protocol version is supported (AHP `-32005`).
    UnsupportedProtocolVersion { offered: Vec<String> },
    /// A referenced resource/id does not exist (`-32008`).
    NotFound(String),
    /// The action or path is outside what this client may touch (`-32009`).
    PermissionDenied(String),
    /// Creation collided with an existing resource (`-32010`).
    AlreadyExists(String),
    /// A concurrent update won (`-32011`).
    Conflict(String),
    /// Malformed params for a declared method (`-32602`).
    InvalidParams(String),
    /// Unknown method name (`-32601`).
    MethodNotFound(String),
    /// A declared x-manox surface that this build does not serve (`-32080`).
    Unimplemented(String),
    /// A dispatched action refused by [`crate::ext::accepts_action`].
    ActionRejected(String),
    /// The `resource*` plane refused a path (`-32082`).
    ResourceDenied(String),
    /// The runtime backend failed (`-32083`).
    Backend(String),
}

impl HostError {
    /// The stable code for this failure (see [`codes::DECLARED`]).
    pub fn code(&self) -> i32 {
        match self {
            Self::SessionNotFound(_) => codes::ahp::SESSION_NOT_FOUND,
            Self::SessionAlreadyExists(_) => codes::ahp::SESSION_ALREADY_EXISTS,
            Self::ProviderNotFound(_) => codes::ahp::PROVIDER_NOT_FOUND,
            Self::TurnInProgress(_) => codes::ahp::TURN_IN_PROGRESS,
            Self::UnsupportedProtocolVersion { .. } => codes::ahp::UNSUPPORTED_PROTOCOL_VERSION,
            Self::NotFound(_) => codes::ahp::NOT_FOUND,
            Self::PermissionDenied(_) => codes::ahp::PERMISSION_DENIED,
            Self::AlreadyExists(_) => codes::ahp::ALREADY_EXISTS,
            Self::Conflict(_) => codes::ahp::CONFLICT,
            Self::InvalidParams(_) => codes::rpc::INVALID_PARAMS,
            Self::MethodNotFound(_) => codes::rpc::METHOD_NOT_FOUND,
            Self::Unimplemented(_) => codes::X_MANOX_UNSUPPORTED,
            Self::ActionRejected(_) => codes::X_MANOX_ACTION_REJECTED,
            Self::ResourceDenied(_) => codes::X_MANOX_RESOURCE_DENIED,
            Self::Backend(_) => codes::X_MANOX_BACKEND,
        }
    }

    /// Human-readable message (English; the runtime never localises).
    pub fn message(&self) -> String {
        match self {
            Self::SessionNotFound(id) => format!("session not found: {id}"),
            Self::SessionAlreadyExists(id) => format!("session already exists: {id}"),
            Self::ProviderNotFound(name) => format!("provider not found: {name}"),
            Self::TurnInProgress(id) => format!("turn already in progress: {id}"),
            Self::UnsupportedProtocolVersion { offered } => {
                format!("no supported protocol version among {offered:?}")
            }
            Self::NotFound(what) => format!("not found: {what}"),
            Self::PermissionDenied(what) => format!("permission denied: {what}"),
            Self::AlreadyExists(what) => format!("already exists: {what}"),
            Self::Conflict(what) => format!("conflict: {what}"),
            Self::InvalidParams(what) => format!("invalid params: {what}"),
            Self::MethodNotFound(method) => format!("unknown method: {method}"),
            Self::Unimplemented(name) => format!("declared but not implemented: {name}"),
            Self::ActionRejected(reason) => format!("action rejected: {reason}"),
            Self::ResourceDenied(path) => format!("resource path denied: {path}"),
            Self::Backend(msg) => format!("backend failure: {msg}"),
        }
    }

    /// Optional structured payload (required for `-32005`).
    pub fn data(&self) -> Option<Value> {
        match self {
            Self::UnsupportedProtocolVersion { .. } => Some(json!({
                "supportedVersions": ahp_types::version::SUPPORTED_PROTOCOL_VERSIONS,
            })),
            _ => None,
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for HostError {}

impl From<HostError> for JsonRpcError {
    fn from(err: HostError) -> Self {
        JsonRpcError {
            code: err.code(),
            message: err.message(),
            data: err.data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_variant_carries_a_declared_code() {
        let samples = [
            HostError::SessionNotFound("s".into()),
            HostError::SessionAlreadyExists("s".into()),
            HostError::ProviderNotFound("p".into()),
            HostError::TurnInProgress("s".into()),
            HostError::UnsupportedProtocolVersion {
                offered: vec!["9.9.9".into()],
            },
            HostError::NotFound("x".into()),
            HostError::PermissionDenied("x".into()),
            HostError::AlreadyExists("x".into()),
            HostError::Conflict("x".into()),
            HostError::InvalidParams("x".into()),
            HostError::MethodNotFound("x".into()),
            HostError::Unimplemented("x".into()),
            HostError::ActionRejected("x".into()),
            HostError::ResourceDenied("x".into()),
            HostError::Backend("x".into()),
        ];
        let declared: Vec<i32> = codes::DECLARED.iter().map(|(_, code)| *code).collect();
        for err in samples {
            assert!(
                declared.contains(&err.code()),
                "undeclared code {} for {err:?}",
                err.code()
            );
        }
    }

    #[test]
    fn unsupported_version_error_carries_supported_versions() {
        let err = HostError::UnsupportedProtocolVersion {
            offered: vec!["9.9.9".into()],
        };
        let data = err.data().expect("required data");
        assert!(data["supportedVersions"].is_array());
    }
}
