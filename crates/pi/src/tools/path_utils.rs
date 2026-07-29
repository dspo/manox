// Path utilities — path resolution and validation.
//
// Resolves paths relative to the working directory, handles `~` expansion,
// and validates that paths stay within allowed boundaries.

use std::path::{Path, PathBuf};

/// Resolve a potentially relative path against the working directory.
///
/// Handles:
/// - Absolute paths (returned as-is after canonicalization)
/// - `~` home directory expansion
/// - Relative paths (resolved against `cwd`)
pub fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_tilde(path_str);
    let path = Path::new(&expanded);

    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Expand `~` to the user's home directory.
fn expand_tilde(path_str: &str) -> String {
    if path_str.starts_with("~/") {
        if let Some(home) = dirs_home() {
            return home.to_string_lossy().to_string() + &path_str[1..];
        }
    } else if path_str == "~" {
        if let Some(home) = dirs_home() {
            return home.to_string_lossy().to_string();
        }
    }
    path_str.to_string()
}

/// Get the user's home directory.
fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(target_os = "windows")]
            {
                std::env::var("USERPROFILE").ok().map(PathBuf::from)
            }
            #[cfg(not(target_os = "windows"))]
            {
                None
            }
        })
}

/// Check whether a path is within an allowed directory.
///
/// Returns true if the path is under `allowed_root` (or equal to it).
pub fn is_within(path: &Path, allowed_root: &Path) -> bool {
    // Canonicalize both paths for comparison.
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = allowed_root
        .canonicalize()
        .unwrap_or_else(|_| allowed_root.to_path_buf());

    canonical_path.starts_with(&canonical_root)
}

/// Resolve a path and validate it's within the working directory.
///
/// Returns an error if the resolved path escapes the working directory
/// (e.g., via `../` traversal).
pub fn resolve_safe(path_str: &str, cwd: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_path(path_str, cwd);

    // Canonicalize to resolve symlinks and `..`.
    let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    if !canonical.starts_with(&cwd_canonical) {
        return Err(format!(
            "Path escapes working directory: {} → {}",
            path_str,
            canonical.display()
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_absolute_path() {
        let resolved = resolve_path("/usr/bin", Path::new("/tmp"));
        assert_eq!(resolved, PathBuf::from("/usr/bin"));
    }

    #[test]
    fn test_resolve_relative_path() {
        let resolved = resolve_path("src/main.rs", Path::new("/project"));
        assert_eq!(resolved, PathBuf::from("/project/src/main.rs"));
    }

    #[test]
    fn test_is_within() {
        assert!(is_within(
            Path::new("/project/src/main.rs"),
            Path::new("/project")
        ));
    }

    #[test]
    fn test_is_not_within() {
        assert!(!is_within(
            Path::new("/etc/passwd"),
            Path::new("/project")
        ));
    }
}