//! The `resource*` file plane.
//!
//! AHP offloads large content by reference: a `contentRef` response part or a
//! referenced tool input names a URI, and the client fetches it with
//! `resourceRead`. The host therefore has to expose *some* file plane. manox
//! already fences tool access to granted working directories, and the plane
//! here inherits that fence: only paths under the session's granted roots can
//! be read or written, everything else fails closed with
//! [`crate::codes::X_MANOX_RESOURCE_DENIED`].

use ahp_types::commands::{
    ResourceListResult, ResourceReadParams, ResourceReadResult, ResourceWriteMode,
    ResourceWriteParams,
};
use ahp_types::common::Uri;

use crate::error::HostError;

/// The file plane a [`crate::Backend`] may serve.
pub trait ResourcePlane: Send + Sync + 'static {
    /// Read a resource (optionally a range).
    fn read(&self, params: &ResourceReadParams) -> Result<ResourceReadResult, HostError>;

    /// Write a resource.
    fn write(&self, params: &ResourceWriteParams) -> Result<(), HostError>;

    /// List a directory.
    fn list(&self, uri: &Uri) -> Result<ResourceListResult, HostError>;

    /// Delete a resource.
    fn delete(&self, uri: &Uri) -> Result<(), HostError>;

    /// Write mode of the last write (declared by AHP as `ResourceWriteMode`);
    /// the plane itself decides whether to honour it.
    fn default_write_mode(&self) -> ResourceWriteMode;
}

/// A plane that serves nothing — the default for backends without file access.
pub struct NoResources;

impl ResourcePlane for NoResources {
    fn read(&self, _params: &ResourceReadParams) -> Result<ResourceReadResult, HostError> {
        Err(HostError::Unimplemented("resourceRead".into()))
    }

    fn write(&self, _params: &ResourceWriteParams) -> Result<(), HostError> {
        Err(HostError::Unimplemented("resourceWrite".into()))
    }

    fn list(&self, _uri: &Uri) -> Result<ResourceListResult, HostError> {
        Err(HostError::Unimplemented("resourceList".into()))
    }

    fn delete(&self, _uri: &Uri) -> Result<(), HostError> {
        Err(HostError::Unimplemented("resourceDelete".into()))
    }

    fn default_write_mode(&self) -> ResourceWriteMode {
        ResourceWriteMode::Truncate
    }
}
