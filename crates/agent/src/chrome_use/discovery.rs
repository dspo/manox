//! Chromium executable discovery — scans the machine for available
//! Chrome/Chromium binaries the engine could launch. Returns all matches
//! (not just the first) so the agent can make an informed choice.
//!
//! Scan order: env vars → Playwright browser cache → well-known system
//! install paths → `PATH` lookup. Every candidate is reported with its
//! source and variant instead of short-circuiting on the first hit.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// One discovered Chromium executable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChromiumCandidate {
    /// Absolute path to the executable.
    pub path: String,
    /// Where it was found: `"env"`, `"playwright-cache"`, `"system"`, `"path"`.
    pub source: String,
    /// Browser identity: `"for-testing"`, `"stable"`, `"canary"`,
    /// `"chromium"`, `"edge"`, `"brave"`.
    pub variant: String,
}

/// Scan the machine for all Chromium executables, in priority order.
pub fn discover_chromium_executables() -> Vec<ChromiumCandidate> {
    let mut results = Vec::new();

    for key in ["RUSTWRIGHT_CHROMIUM", "CHROME", "CHROMIUM"] {
        if let Ok(path) = std::env::var(key) {
            let p = PathBuf::from(&path);
            if is_executable_file(&p) {
                results.push(ChromiumCandidate {
                    path: path.clone(),
                    source: "env".into(),
                    variant: variant_name(&p, key),
                });
            }
        }
    }

    // 2. Playwright cache directories.
    for cache_dir in browser_cache_dirs() {
        let Ok(read_dir) = std::fs::read_dir(&cache_dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
        // Newest first, matching the engine's own sort.
        entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        for path in &entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if let Some(c) = resolve_playwright_entry(path, name) {
                results.push(c);
            }
        }
    }

    // 3. Well-known system paths.
    for candidate in system_candidates() {
        if is_executable_file(Path::new(&candidate.path)) {
            results.push(candidate);
        }
    }

    // 4. PATH lookup.
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "msedge",
        "microsoft-edge",
    ] {
        if let Some(path) = find_on_path(name) {
            results.push(ChromiumCandidate {
                path: path.to_string_lossy().into_owned(),
                source: "path".into(),
                variant: variant_name(&path, name),
            });
        }
    }

    dedup(results)
}

fn resolve_playwright_entry(dir: &Path, name: &str) -> Option<ChromiumCandidate> {
    if !name.starts_with("chromium") {
        return None;
    }
    let variant = "for-testing";
    let binary = if cfg!(target_os = "macos") {
        if name.starts_with("chromium_headless_shell") {
            dir.join("chrome-headless-shell-mac-arm64/chrome-headless-shell")
        } else {
            dir.join(
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            )
        }
    } else if cfg!(target_os = "windows") {
        if name.starts_with("chromium_headless_shell") {
            dir.join("chrome-headless-shell-win64/chrome-headless-shell.exe")
        } else {
            dir.join("chrome-win64/chrome.exe")
        }
    } else if name.starts_with("chromium_headless_shell") {
        dir.join("chrome-headless-shell-linux64/chrome-headless-shell")
    } else {
        dir.join("chrome-linux64/chrome")
    };
    if is_executable_file(&binary) {
        Some(ChromiumCandidate {
            path: binary.to_string_lossy().into_owned(),
            source: "playwright-cache".into(),
            variant: variant.into(),
        })
    } else {
        None
    }
}

fn system_candidates() -> Vec<ChromiumCandidate> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let paths: &[(&str, &str)] = &[
            (
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "stable",
            ),
            (
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "chromium",
            ),
            (
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                "edge",
            ),
            (
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
                "brave",
            ),
        ];
        for (p, v) in paths {
            out.push(ChromiumCandidate {
                path: p.to_string(),
                source: "system".into(),
                variant: v.to_string(),
            });
        }
        if let Some(home) = home_dir() {
            for (suffix, variant) in [
                (
                    "Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                    "stable",
                ),
                (
                    "Applications/Chromium.app/Contents/MacOS/Chromium",
                    "chromium",
                ),
            ] {
                out.push(ChromiumCandidate {
                    path: home.join(suffix).to_string_lossy().into_owned(),
                    source: "system".into(),
                    variant: variant.to_string(),
                });
            }
        }
    } else if cfg!(target_os = "windows") {
        for env_key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(root) = std::env::var(env_key) {
                out.push(ChromiumCandidate {
                    path: format!("{root}/Google/Chrome/Application/chrome.exe"),
                    source: "system".into(),
                    variant: "stable".into(),
                });
            }
        }
    } else {
        for (p, v) in [
            ("/usr/bin/chromium", "chromium"),
            ("/usr/bin/chromium-browser", "chromium"),
            ("/usr/bin/google-chrome", "stable"),
            ("/usr/bin/google-chrome-stable", "stable"),
            ("/snap/bin/chromium", "chromium"),
        ] {
            out.push(ChromiumCandidate {
                path: p.to_string(),
                source: "system".into(),
                variant: v.to_string(),
            });
        }
    }
    out
}

fn browser_cache_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(path) = std::env::var("RUSTWRIGHT_BROWSERS_PATH")
        && !path.is_empty()
    {
        dirs.push(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("PLAYWRIGHT_BROWSERS_PATH")
        && !path.is_empty()
        && path != "0"
    {
        dirs.push(PathBuf::from(path));
    }
    if let Some(home) = home_dir() {
        dirs.push(if cfg!(target_os = "macos") {
            home.join("Library/Caches/ms-playwright")
        } else if cfg!(target_os = "windows") {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Local"))
                .join("ms-playwright")
        } else {
            home.join(".cache/ms-playwright")
        });
    }
    let mut seen = std::collections::HashSet::new();
    dirs.into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn variant_name(path: &Path, _hint: &str) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.contains("for testing") || name.contains("headless-shell") {
        "for-testing".into()
    } else if name.contains("edge") || name.contains("msedge") {
        "edge".into()
    } else if name.contains("brave") {
        "brave".into()
    } else if name.contains("chromium") {
        "chromium".into()
    } else if name.contains("canary") {
        "canary".into()
    } else {
        "stable".into()
    }
}

fn dedup(candidates: Vec<ChromiumCandidate>) -> Vec<ChromiumCandidate> {
    let mut seen = std::collections::HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.path.clone()))
        .collect()
}
