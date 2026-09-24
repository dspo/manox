//! The `resource*` file plane, fenced to the session's granted roots.
//!
//! AHP offloads large content by reference: a `contentRef` response part, a
//! pasted image, or a referenced tool input names a URI the client fetches with
//! `resourceRead`. Served naively that is arbitrary remote file access on the
//! host machine, so this plane inherits the same fence the kernel puts in front
//! of tool file effects: a path is reachable only when it resolves **under one
//! of the session's granted working directories**.
//!
//! # The fence
//!
//! Every path is canonicalized *before* it is checked and used, so `..`, a
//! symlink, or a not-yet-existing leaf cannot walk outside a granted root. The
//! check is `starts_with` on the canonical path, which is a component-wise
//! comparison — `/work/grants-evil` does not pass for the root `/work/grants`.
//!
//! A read is additionally allowed under the manox home: the host's own
//! journals and session files are content a client legitimately renders (a
//! transcript entry, a plan artefact) and none of it is outside the user's own
//! state root.
//!
//! Writes are held to the granted roots only. Creating a file through a
//! canonicalized *parent* is the one subtlety: the leaf does not exist yet, so
//! the parent is what gets resolved, and the joined name is re-checked.

use std::path::{Path, PathBuf};

use ahp_types::commands::{
    DirectoryEntry, ResourceListResult, ResourceReadParams, ResourceReadResult, ResourceWriteMode,
    ResourceWriteParams,
};
use ahp_types::common::Uri;

use manox_ahp::error::HostError;
use manox_ahp::resource::ResourcePlane;

/// The fence a resource request is checked against.
#[derive(Debug, Clone, Default)]
pub struct Fence {
    /// Directories the session's agent may touch (cwd plus extra grants).
    roots: Vec<PathBuf>,
    /// The user's own state root, readable (never writable) by the client.
    home: Option<PathBuf>,
}

impl Fence {
    /// A fence over `roots`, with `home` additionally readable.
    ///
    /// Roots are canonicalized **here**, once, because a requested path is
    /// always canonicalized before it is compared: a root left in its
    /// as-written form would not match its own descendants whenever any
    /// component is a symlink. That is not hypothetical — macOS `/var` is a
    /// symlink to `/private/var`, so a scratch root under `/var/folders/...`
    /// silently matched nothing, and the failure direction was a *denial* of
    /// legitimate access that no test caught because the tests built fences
    /// from already-canonical paths.
    ///
    /// A root that cannot be canonicalized (it does not exist yet) is kept
    /// as-is rather than dropped: refusing to build the fence would turn a
    /// missing directory into "every path denied", which is a worse failure
    /// than a root that is merely compared unnormalized.
    pub fn new(roots: Vec<PathBuf>, home: Option<PathBuf>) -> Self {
        let canonical = |path: PathBuf| canonicalize_allow_missing(&path).unwrap_or(path);
        Self {
            roots: roots.into_iter().map(canonical).collect(),
            home: home.map(canonical),
        }
    }

    /// Whether `path` is under a granted root (canonicalized).
    fn writable(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    /// Whether `path` may be read: a granted root, or the state root.
    fn readable(&self, path: &Path) -> bool {
        self.writable(path)
            || self
                .home
                .as_ref()
                .is_some_and(|home| path.starts_with(home))
    }

    /// Resolve a `file://` URI to a canonical path, or the host error the plane
    /// answers with.
    ///
    /// A path that does not exist is canonicalized through its nearest existing
    /// ancestor, so a create is fenced on where the file *would* land rather
    /// than on a path that cannot be resolved at all.
    fn resolve(&self, uri: &str) -> Result<PathBuf, HostError> {
        let raw = uri
            .strip_prefix("file://")
            .ok_or_else(|| HostError::ResourceDenied(format!("not a file URI: {uri}")))?;
        let path = PathBuf::from(raw);
        let path = if path.is_absolute() {
            path
        } else {
            return Err(HostError::ResourceDenied(format!(
                "resource paths must be absolute: {uri}"
            )));
        };
        canonicalize_allow_missing(&path)
            .map_err(|error| HostError::ResourceDenied(format!("{uri}: {error}")))
    }
}

/// Canonicalize `path`, resolving a missing leaf through its nearest existing
/// ancestor so a create is still checked against the real filesystem location.
fn canonicalize_allow_missing(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match cursor.parent() {
            Some(parent) => {
                let Some(name) = cursor.file_name() else {
                    return path.canonicalize();
                };
                missing.push(name.to_os_string());
                if let Ok(canonical) = parent.canonicalize() {
                    let mut resolved = canonical;
                    for name in missing.iter().rev() {
                        resolved.push(name);
                    }
                    return Ok(resolved);
                }
                cursor = parent;
            }
            None => return path.canonicalize(),
        }
    }
}

/// The runtime's `resource*` plane.
pub struct RuntimeResources {
    fence: Fence,
}

impl RuntimeResources {
    /// A plane fenced to `roots`, with the user's state root readable.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let home = manox_agent::paths::manox_config_dir().ok();
        Self {
            fence: Fence::new(roots, home),
        }
    }

    /// The bytes a `data` field carries, per its declared encoding.
    fn decode(
        data: &str,
        encoding: ahp_types::commands::ContentEncoding,
    ) -> Result<Vec<u8>, HostError> {
        use ahp_types::commands::ContentEncoding;
        match encoding {
            ContentEncoding::Utf8 => Ok(data.as_bytes().to_vec()),
            ContentEncoding::Base64 => manox_journal::base64_bytes::decode(data)
                .map_err(|error| HostError::InvalidParams(format!("bad base64: {error}"))),
        }
    }
}

impl ResourcePlane for RuntimeResources {
    fn read(&self, params: &ResourceReadParams) -> Result<ResourceReadResult, HostError> {
        let path = self.fence.resolve(&params.uri)?;
        if !self.fence.readable(&path) {
            return Err(HostError::ResourceDenied(format!(
                "{} is outside the session's granted directories",
                params.uri
            )));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| HostError::NotFound(format!("{}: {error}", params.uri)))?;
        // Honour the requested encoding when we can, and fall back to what the
        // bytes actually are otherwise (AHP requires a fallback, not a failure).
        let utf8 = String::from_utf8(bytes.clone()).ok();
        match (params.encoding, utf8) {
            (Some(ahp_types::commands::ContentEncoding::Utf8), Some(text)) => {
                Ok(ResourceReadResult {
                    data: text,
                    encoding: ahp_types::commands::ContentEncoding::Utf8,
                    content_type: content_type(&path),
                })
            }
            (_, Some(text)) if params.encoding.is_none() && looks_textual(&path) => {
                Ok(ResourceReadResult {
                    data: text,
                    encoding: ahp_types::commands::ContentEncoding::Utf8,
                    content_type: content_type(&path),
                })
            }
            _ => Ok(ResourceReadResult {
                data: manox_journal::base64_bytes::encode(&bytes),
                encoding: ahp_types::commands::ContentEncoding::Base64,
                content_type: content_type(&path),
            }),
        }
    }

    fn write(&self, params: &ResourceWriteParams) -> Result<(), HostError> {
        let path = self.fence.resolve(params.uri.as_str())?;
        if !self.fence.writable(&path) {
            // Writes get the narrower fence: the state root is the host's own
            // bookkeeping, not client-writable surface.
            return Err(HostError::ResourceDenied(format!(
                "{} is outside the session's granted directories",
                params.uri
            )));
        }
        let bytes = Self::decode(&params.data, params.encoding)?;
        if params.create_only.unwrap_or(false) && path.exists() {
            return Err(HostError::AlreadyExists(params.uri.to_string()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| HostError::Backend(format!("{}: {error}", params.uri)))?;
        }
        let existing = if path.exists() {
            std::fs::read(&path).unwrap_or_default()
        } else {
            Vec::new()
        };
        let merged = match params.mode.unwrap_or(ResourceWriteMode::Truncate) {
            ResourceWriteMode::Truncate => {
                let at = params.position.unwrap_or(0).max(0) as usize;
                let mut out = existing[..at.min(existing.len())].to_vec();
                out.extend_from_slice(&bytes);
                out
            }
            ResourceWriteMode::Append => {
                let offset = params.position.unwrap_or(0).max(0) as usize;
                let at = existing.len().saturating_sub(offset);
                let mut out = existing[..at].to_vec();
                out.extend_from_slice(&bytes);
                out.extend_from_slice(&existing[at..]);
                out
            }
            ResourceWriteMode::Insert => {
                let at = params.position.unwrap_or(0).max(0) as usize;
                let at = at.min(existing.len());
                let mut out = existing[..at].to_vec();
                out.extend_from_slice(&bytes);
                out.extend_from_slice(&existing[at..]);
                out
            }
        };
        std::fs::write(&path, merged)
            .map_err(|error| HostError::Backend(format!("{}: {error}", params.uri)))
    }

    fn list(&self, uri: &Uri) -> Result<ResourceListResult, HostError> {
        let path = self.fence.resolve(uri.as_str())?;
        if !self.fence.readable(&path) {
            return Err(HostError::ResourceDenied(format!(
                "{uri} is outside the session's granted directories"
            )));
        }
        let mut entries = Vec::new();
        let read = std::fs::read_dir(&path)
            .map_err(|error| HostError::NotFound(format!("{uri}: {error}")))?;
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let kind = match entry.file_type() {
                Ok(file) if file.is_dir() => "directory",
                Ok(_) => "file",
                Err(_) => continue,
            };
            entries.push(DirectoryEntry {
                name,
                r#type: kind.to_string(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ResourceListResult { entries })
    }

    fn delete(&self, uri: &Uri) -> Result<(), HostError> {
        let path = self.fence.resolve(uri.as_str())?;
        if !self.fence.writable(&path) {
            return Err(HostError::ResourceDenied(format!(
                "{uri} is outside the session's granted directories"
            )));
        }
        std::fs::remove_file(&path).map_err(|error| HostError::NotFound(format!("{uri}: {error}")))
    }

    fn default_write_mode(&self) -> ResourceWriteMode {
        ResourceWriteMode::Truncate
    }
}

/// A content type for the extensions worth naming; `None` lets the client guess.
fn content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let kind = match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        _ => return None,
    };
    Some(kind.to_string())
}

/// Whether an extension names text, so an unencoded read answers as `utf-8`
/// rather than forcing every client to base64-decode source files.
fn looks_textual(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return true;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "txt"
            | "md"
            | "json"
            | "jsonl"
            | "toml"
            | "yaml"
            | "yml"
            | "rs"
            | "ts"
            | "js"
            | "tsx"
            | "jsx"
            | "py"
            | "sh"
            | "log"
            | "csv"
            | "html"
            | "css"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manox-ahp-resources-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A plane over one root, through the real constructor — the fence's own
    /// canonicalization (see [`Fence::new`]) is part of what is under test, so
    /// the tests must not pre-canonicalize the root for it.
    fn plane(root: &Path) -> RuntimeResources {
        RuntimeResources {
            fence: Fence::new(vec![root.to_path_buf()], None),
        }
    }

    fn read_params(uri: &str) -> ResourceReadParams {
        ResourceReadParams {
            channel: ahp_types::common::ROOT_RESOURCE_URI.to_string(),
            meta: None,
            uri: uri.to_string(),
            encoding: None,
        }
    }

    fn write_params(uri: &str, data: &str) -> ResourceWriteParams {
        ResourceWriteParams {
            channel: ahp_types::common::ROOT_RESOURCE_URI.to_string(),
            meta: None,
            uri: uri.to_string(),
            data: data.to_string(),
            encoding: ahp_types::commands::ContentEncoding::Utf8,
            content_type: None,
            create_only: None,
            mode: None,
            position: None,
            if_match: None,
        }
    }

    #[test]
    fn a_path_outside_the_granted_root_is_denied() {
        let root = scratch();
        let outside = scratch();
        std::fs::write(outside.join("secret.txt"), "no").unwrap();
        let plane = plane(&root);

        let err = plane
            .read(&read_params(&format!(
                "file://{}/secret.txt",
                outside.display()
            )))
            .expect_err("outside the fence");
        assert_eq!(err.code(), manox_ahp::codes::X_MANOX_RESOURCE_DENIED);
    }

    /// A prefix that merely *looks* like a root is not one: the check is
    /// component-wise, not string-wise.
    #[test]
    fn a_sibling_sharing_the_root_prefix_is_denied() {
        let base = scratch();
        let root = base.join("grants");
        let sibling = base.join("grants-evil");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("x.txt"), "no").unwrap();
        let plane = plane(&root);
        assert!(
            plane
                .read(&read_params(&format!("file://{}/x.txt", sibling.display())))
                .is_err()
        );
    }

    /// `..` must not walk out of a granted root.
    #[test]
    fn dot_dot_cannot_escape_the_fence() {
        let base = scratch();
        let root = base.join("grants");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(base.join("outside.txt"), "no").unwrap();
        let plane = plane(&root);
        assert!(
            plane
                .read(&read_params(&format!(
                    "file://{}/../outside.txt",
                    root.display()
                )))
                .is_err(),
            "traversal must be resolved before the fence check"
        );
    }

    #[test]
    fn a_read_inside_the_root_succeeds() {
        let root = scratch();
        std::fs::write(root.join("note.md"), "# hi").unwrap();
        let plane = plane(&root);
        let result = plane
            .read(&read_params(&format!("file://{}/note.md", root.display())))
            .expect("inside the fence");
        assert_eq!(result.encoding, ahp_types::commands::ContentEncoding::Utf8);
        assert_eq!(result.data, "# hi");
        assert_eq!(result.content_type.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn a_write_can_create_a_new_file_inside_the_root() {
        let root = scratch();
        let plane = plane(&root);
        let target = root.join("nested").join("out.txt");
        plane
            .write(&write_params(
                &format!("file://{}", target.display()),
                "written",
            ))
            .expect("inside the fence");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "written");
    }

    #[test]
    fn a_write_outside_the_root_is_denied() {
        let base = scratch();
        let root = base.join("grants");
        std::fs::create_dir_all(&root).unwrap();
        let plane = plane(&root);
        let target = base.join("outside.txt");
        let err = plane
            .write(&write_params(&format!("file://{}", target.display()), "no"))
            .expect_err("outside the fence");
        assert_eq!(err.code(), manox_ahp::codes::X_MANOX_RESOURCE_DENIED);
        assert!(!target.exists(), "a denied write must not touch the disk");
    }

    #[test]
    fn create_only_refuses_an_existing_file() {
        let root = scratch();
        std::fs::write(root.join("taken.txt"), "old").unwrap();
        let plane = plane(&root);
        let mut params = write_params(&format!("file://{}/taken.txt", root.display()), "new");
        params.create_only = Some(true);
        let err = plane.write(&params).expect_err("already exists");
        assert_eq!(err.code(), manox_ahp::codes::ahp::ALREADY_EXISTS);
        assert_eq!(
            std::fs::read_to_string(root.join("taken.txt")).unwrap(),
            "old"
        );
    }

    #[test]
    fn list_names_entries_sorted() {
        let root = scratch();
        std::fs::write(root.join("b.txt"), "").unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        let plane = plane(&root);
        let result = plane
            .list(&Uri::from(format!("file://{}", root.display())))
            .expect("inside the fence");
        let names: Vec<&str> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "dir"]);
        assert_eq!(result.entries[2].r#type, "directory");
    }

    /// The read fence is *wider* than the write fence: the session's own state
    /// root is content a client legitimately renders (a journal entry, a plan
    /// artefact), so it is readable — but never writable.
    ///
    /// This is the one relaxation in the plane, and `Fence::new` is what
    /// applies it, so the test drives the real constructor rather than the
    /// hand-built fence the other cases use. A relaxation with no test is the
    /// worst kind of untested code: the failure mode is silent over-permission.
    #[test]
    fn the_state_root_is_readable_but_never_writable() {
        // `hermetic_home` points HOME at a throwaway directory, so the state
        // root is a scratch path and the developer's real ~/.manox is never
        // touched.
        let _guards = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        let home = manox_agent::paths::manox_config_dir().expect("a hermetic HOME");
        let plans = home.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("kept.md"), "# plan").unwrap();

        // No granted roots at all: everything reachable must be reachable
        // *because* it is the state root.
        let plane = RuntimeResources::new(Vec::new());
        let result = plane
            .read(&read_params(&format!("file://{}/kept.md", plans.display())))
            .expect("the state root is readable");
        assert_eq!(result.data, "# plan");

        // The same path is not writable: the state root is the host's own
        // bookkeeping, not client-writable surface.
        let err = plane
            .write(&write_params(
                &format!("file://{}/plans/planted.md", home.display()),
                "no",
            ))
            .expect_err("the state root is not writable");
        assert_eq!(err.code(), manox_ahp::codes::X_MANOX_RESOURCE_DENIED);
        assert!(
            !home.join("plans").join("planted.md").exists(),
            "a denied write must not touch the disk"
        );

        // And something outside both fences stays denied even for reads.
        let outside = scratch();
        std::fs::write(outside.join("x.txt"), "no").unwrap();
        assert!(
            plane
                .read(&read_params(&format!("file://{}/x.txt", outside.display())))
                .is_err(),
            "the state root is a widening, not the removal of the fence"
        );
    }

    #[test]
    fn a_non_file_uri_is_denied() {
        let plane = plane(&scratch());
        assert!(plane.read(&read_params("https://example.com/x")).is_err());
    }
}
