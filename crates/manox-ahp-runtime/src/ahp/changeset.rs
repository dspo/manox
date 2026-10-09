//! The changeset engine — the AHP face of uncommitted working-tree changes.
//!
//! One static changeset per session (`ahp-changeset:/<session-id>/uncommitted`,
//! `changeKind: "uncommitted"`), spanning every working directory the session
//! was granted: the protocol's multiroot semantics say a session-wide
//! changeset naturally crosses directory roots, so the catalogue carries a
//! single spanning entry rather than one per root.
//!
//! The data source is a git scan over those directories (`git status
//! --porcelain=v1 -z` + per-file diffs), shelled out to the system git — the
//! same sanctioned pattern the subagent worktree machinery uses; the `git2`
//! crate stays banned. A directory that is not inside a git repository
//! contributes nothing, and a session with no repositories at all advertises
//! no catalogue entry: absent is the honest answer, not an empty chip.
//!
//! The engine is the review authority: `changeset/filesReviewChanged` records
//! the reviewer's flags here, and a recompute carries them over — cleared per
//! the spec for files whose patch changed under the stable id.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahp_types::actions::{
    ChangesetContentChangedAction, ChangesetStatusChangedAction, StateAction,
};
use ahp_types::common::JsonObject;
use ahp_types::state::{
    ChangesSummary, Changeset, ChangesetCapabilities, ChangesetFile, ChangesetOperation,
    ChangesetOperationScope, ChangesetOperationStatus, ChangesetState, ChangesetStatus, ContentRef,
    ErrorInfo, FileEdit, FileEditDiffStats, FileEditSide,
};
use parking_lot::Mutex;
use serde_json::Value;

/// The changeset key this engine serves. One kind, deliberately: turn and
/// branch slices need per-turn file instrumentation the journal does not
/// carry yet.
pub const KEY: &str = "uncommitted";

/// `ahp-changeset:/<session-id>/uncommitted` — the static template is the
/// expanded URI itself.
pub fn uri(session_id: &str) -> String {
    format!("ahp-changeset:/{session_id}/{KEY}")
}

/// The (session id, changeset key) inside a changeset channel URI.
pub fn parse(uri: &str) -> Option<(String, &str)> {
    let rest = uri.strip_prefix("ahp-changeset:/")?;
    let (session_id, key) = rest.split_once('/')?;
    (!session_id.is_empty() && !key.is_empty()).then(|| (session_id.to_string(), key))
}

/// The session catalogue entry, once a scan has confirmed at least one git
/// repository among the working directories.
fn catalogue_entry(session_id: &str) -> Changeset {
    Changeset {
        label: "Uncommitted Changes".to_string(),
        uri_template: uri(session_id),
        description: Some(
            "Everything the working trees carry that HEAD does not, across all of this session's \
             directories."
                .to_string(),
        ),
        change_kind: KEY.to_string(),
        capabilities: Some(ChangesetCapabilities {
            review: Some(JsonObject::new()),
        }),
    }
}

/// The one operation this changeset offers: discard the uncommitted changes —
/// whole-set or per file. Destructive, hence the confirmation contract.
fn revert_operation() -> ChangesetOperation {
    ChangesetOperation {
        id: "revert".to_string(),
        label: "Revert".to_string(),
        description: Some(
            "Restore the selected paths to HEAD (untracked files are deleted).".to_string(),
        ),
        scopes: vec![
            ChangesetOperationScope::Changeset,
            ChangesetOperationScope::Resource,
        ],
        confirmation: Some(
            "This discards uncommitted changes to the selected paths. It cannot be undone."
                .to_string()
                .into(),
        ),
        icon: Some("trash".to_string()),
        group: None,
        status: ChangesetOperationStatus::Idle,
        error: None,
    }
}

/// Patch bodies above this many bytes are truncated (with a `_meta` note) —
/// a minified bundle in the working tree must not flood a subscriber.
const PATCH_CAP: usize = 100 * 1024;

/// One scanned file, before it becomes a wire `ChangesetFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawFile {
    /// The wire id: `file://` + the absolute worktree path.
    id: String,
    patch: String,
    added: u64,
    removed: u64,
    /// The worktree deleted the file.
    deleted: bool,
}

fn raw_file(path: PathBuf, patch: String, added: u64, removed: u64, deleted: bool) -> RawFile {
    RawFile {
        id: format!("file://{}", path.display()),
        patch,
        added,
        removed,
        deleted,
    }
}

/// A finished git invocation — exit status included, because some callers
/// care (`diff --no-index` exits 1 on a *successful* diff).
struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// The git access seam — a trait so the state machine tests run without a
/// repository. The real runner shells out to the system git.
trait GitRunner: Send + Sync {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, String>;

    fn stdout(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        let out = self.run(cwd, args)?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(out.stderr)
        }
    }
}

struct SystemGit;

impl GitRunner for SystemGit {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// One completed scan: the files plus how many repository roots answered.
/// The count is what makes the catalogue honest — a session whose
/// directories are all outside any repository has an *empty* scan and no
/// catalogue entry, which are different facts.
struct Scan {
    files: Vec<RawFile>,
    repos: usize,
}

/// Scan the session's directories for uncommitted changes (staged and
/// unstaged together — the changeset is "what HEAD does not have").
fn scan(git: &dyn GitRunner, dirs: &[PathBuf]) -> Result<Scan, String> {
    // Several granted directories may sit in one repository: the repo *root*
    // is the scan unit, deduplicated.
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut not_a_repo = 0;
    for dir in dirs {
        let Ok(root) = git.stdout(dir, &["rev-parse", "--show-toplevel"]) else {
            not_a_repo += 1;
            continue;
        };
        let root = PathBuf::from(root.trim());
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        return if not_a_repo == dirs.len() && !dirs.is_empty() {
            // Every directory is outside any repository: not an error, an
            // empty changeset — the catalogue simply never advertises.
            Ok(Scan {
                files: Vec::new(),
                repos: 0,
            })
        } else {
            Err("git is unavailable".to_string())
        };
    }
    let mut files = Vec::new();
    for root in &roots {
        files.extend(scan_root(git, root)?);
    }
    // The granted roots are the session's fence: git scans whole repos, but
    // a monorepo-style grant (a subdirectory) must not expose — let alone
    // offer to revert — changes elsewhere in the repo. Filtering here keeps
    // the view, the patch contents and the revert target set consistent
    // with the resource plane's fence (an out-of-grant file's content ref
    // would be unfetchable through `resourceRead` anyway).
    files.retain(|file| {
        let path = Path::new(file.id.strip_prefix("file://").unwrap_or(&file.id));
        dirs.iter().any(|dir| path.starts_with(dir))
    });
    files.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Scan {
        files,
        repos: roots.len(),
    })
}

/// Scan one repository root. `-z` keeps paths with spaces and rename pairs
/// lossless (`XY to\0from\0` for renames/copies).
fn scan_root(git: &dyn GitRunner, root: &Path) -> Result<Vec<RawFile>, String> {
    let status = git.stdout(root, &["status", "--porcelain=v1", "-z"])?;
    let mut raw = Vec::new();
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let mut code = field.chars();
        let (Some(x), Some(y)) = (code.next(), code.next()) else {
            continue;
        };
        let path = &field[3..];
        if matches!(x, 'R' | 'C') {
            // The rename/copy source rides as the next NUL field. A rename's
            // source is gone (a deletion entry); a copy's source still
            // exists — only the new file belongs in the changeset.
            let from = fields.next();
            if x == 'R'
                && let Some(from) = from
            {
                raw.push(raw_file(root.join(from), String::new(), 0, 0, true));
            }
        }
        let untracked = x == '?' && y == '?';
        let deleted = x == 'D' || y == 'D';
        let abs = root.join(path);
        let entry = if deleted {
            raw_file(abs, String::new(), 0, 0, true)
        } else if untracked {
            let (patch, added) = synthesized_addition(git, &abs);
            raw_file(abs, patch, added, 0, false)
        } else {
            let patch = git
                .stdout(root, &["diff", "HEAD", "--", path])
                .unwrap_or_default();
            let numstat = git
                .stdout(root, &["diff", "HEAD", "--numstat", "--", path])
                .unwrap_or_default();
            let (added, removed) = parse_numstat(&numstat);
            raw_file(abs, truncate_patch(patch), added, removed, false)
        };
        raw.push(entry);
    }
    Ok(raw)
}

/// An untracked file has no HEAD side: `diff --no-index /dev/null <file>`
/// yields the real all-additions patch (its exit 1 *is* a successful diff).
/// numstat does not know untracked files, so the counts come from the body.
fn synthesized_addition(git: &dyn GitRunner, abs: &Path) -> (String, u64) {
    let out = git
        .run(
            abs.parent().unwrap_or(Path::new("/")),
            &[
                "diff",
                "--no-index",
                "--",
                "/dev/null",
                &abs.to_string_lossy(),
            ],
        )
        .unwrap_or(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: String::new(),
        });
    let patch = truncate_patch(out.stdout);
    let added = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count() as u64;
    (patch, added)
}

/// `added\tremoved\tpath` per line; binary entries (`-\t-`) count as zero.
fn parse_numstat(output: &str) -> (u64, u64) {
    let Some(line) = output.lines().next() else {
        return (0, 0);
    };
    let mut parts = line.split('\t');
    (
        parts.next().and_then(|n| n.parse().ok()).unwrap_or(0),
        parts.next().and_then(|n| n.parse().ok()).unwrap_or(0),
    )
}

fn truncate_patch(patch: String) -> String {
    if patch.len() <= PATCH_CAP {
        patch
    } else {
        let mut cut = PATCH_CAP;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (truncated)", &patch[..cut])
    }
}

/// The wire `ChangesetFile` for one scanned file. `after` points at the live
/// worktree file — a client fetches its content through the existing
/// `resourceRead` plane (the path sits inside the session's granted roots).
/// Deletions carry only `before`: HEAD blobs have no content-serving face, so
/// that side's `ContentRef` is formal — it names a path nothing serves, and
/// the counts tell the deletion story. The patch text rides `_meta`: AHP
/// 1.0.0 typed `FileEdit.diff` down to the two counts, and `_meta` is the
/// protocol's server-defined slot for exactly this kind of payload.
fn wire_file(raw: &RawFile, reviewed: Option<bool>) -> ChangesetFile {
    let side = || FileEditSide {
        uri: raw.id.clone(),
        content: ContentRef {
            uri: raw.id.clone(),
            size_hint: None,
            content_type: None,
            nonce: None,
        },
    };
    let after = (!raw.deleted).then(side);
    let before = raw.deleted.then(side);
    let mut meta = JsonObject::new();
    if !raw.patch.is_empty() {
        meta.insert("patch".to_string(), Value::String(raw.patch.clone()));
    }
    if raw.patch.len() > PATCH_CAP {
        meta.insert("patchTruncated".to_string(), Value::Bool(true));
    }
    ChangesetFile {
        id: raw.id.clone(),
        edit: FileEdit {
            before,
            after,
            diff: Some(FileEditDiffStats {
                added: Some(raw.added as i64),
                removed: Some(raw.removed as i64),
            }),
        },
        reviewed,
        meta: (!meta.is_empty()).then_some(meta),
    }
}

/// One session's changeset: the directories it spans plus the last scan.
struct SessionChangeset {
    dirs: Vec<PathBuf>,
    files: Vec<ChangesetFile>,
    reviewed: HashSet<String>,
    status: ChangesetStatus,
    error: Option<ErrorInfo>,
    /// Whether any scanned directory sat inside a git repository — the
    /// catalogue is advertised only when it does.
    has_repo: bool,
}

impl SessionChangeset {
    fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            files: Vec::new(),
            reviewed: HashSet::new(),
            status: ChangesetStatus::Computing,
            error: None,
            has_repo: false,
        }
    }
}

/// The engine: one changeset per session, computed on demand. All methods
/// are synchronous — a scan is a handful of git invocations (milliseconds) —
/// and callers decide the async wrapper (`block_in_place` on seed paths,
/// `spawn_blocking` on the live recompute).
pub struct Engine {
    sessions: Mutex<HashMap<String, SessionChangeset>>,
    git: Arc<dyn GitRunner>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            git: Arc::new(SystemGit),
        }
    }

    /// The process-global engine, shared by the seed path, the subscribe
    /// ensure, the dispatch arms and the bridge's recompute — one authority,
    /// like the MCP registry.
    pub fn global() -> &'static Engine {
        static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
        ENGINE.get_or_init(Engine::new)
    }

    #[cfg(test)]
    fn with_git(git: Arc<dyn GitRunner>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            git,
        }
    }

    /// The session's catalogue entry, running the first scan when needed.
    /// `None` for a session whose directories hold no git repository.
    ///
    /// Two-phase like [`Engine::recompute`]: the git scan (several process
    /// round trips) runs *outside* the session-table lock, so a first sight
    /// on one session never stalls another session's state read.
    pub fn catalogue(&self, session_id: &str, dirs: Vec<PathBuf>) -> Option<Changeset> {
        // Canonicalize once at entry time: git reports resolved paths
        // (`rev-parse --show-toplevel` resolves symlinks), while the
        // session's recorded cwd may ride them — the restore path compares
        // the two, so they must share one shape.
        let dirs: Vec<PathBuf> = dirs
            .into_iter()
            .map(|dir| dir.canonicalize().unwrap_or(dir))
            .collect();
        let needs_scan = {
            let mut sessions = self.sessions.lock();
            match sessions.get_mut(session_id) {
                Some(entry) => {
                    // Order-insensitive: the seed's directory order is stable
                    // today, but a reorder is not a move and must not pay a
                    // rescan.
                    let mut next = dirs.clone();
                    next.sort();
                    let mut stored = entry.dirs.clone();
                    stored.sort();
                    if next != stored {
                        // The session's directory set moved (a cwd change, a
                        // new grant, or a repository appearing where none
                        // was): rescan against the new set. The entry keeps
                        // its previous completed result while the rescan
                        // runs — that is `Recomputing`, not a first
                        // `Computing`.
                        entry.dirs = next;
                        entry.status = ChangesetStatus::Recomputing;
                    }
                    matches!(
                        entry.status,
                        ChangesetStatus::Computing | ChangesetStatus::Recomputing
                    )
                }
                None => {
                    sessions.insert(session_id.to_string(), SessionChangeset::new(dirs.clone()));
                    true
                }
            }
        };
        if needs_scan {
            let scan = scan(&*self.git, &dirs);
            let mut sessions = self.sessions.lock();
            if let Some(entry) = sessions.get_mut(session_id) {
                match scan {
                    Ok(scan) => {
                        entry.has_repo = scan.repos > 0;
                        entry.files = scan
                            .files
                            .into_iter()
                            .map(|raw| wire_file(&raw, None))
                            .collect();
                        entry.status = ChangesetStatus::Ready;
                    }
                    Err(message) => {
                        entry.status = ChangesetStatus::Error;
                        entry.error = Some(ErrorInfo {
                            error_type: "changeset".to_string(),
                            message,
                            stack: None,
                            meta: None,
                        });
                    }
                }
            }
        }
        let sessions = self.sessions.lock();
        sessions
            .get(session_id)
            .and_then(|entry| entry.has_repo.then(|| catalogue_entry(session_id)))
    }

    /// The chat face of the engine's current state: the catalogue plus the
    /// aggregate line/file counts, for `ChatState.changesets` and
    /// `ChatSummary.changes` (AHP 1.0's per-chat footprint, visible without
    /// subscribing the changeset channel). Read-only over the last scan —
    /// an unknown session or a directory set with no repository answers
    /// `None`, the same "never advertised" shape as [`Engine::state`].
    pub fn chat_face(&self, session_id: &str) -> Option<(Vec<Changeset>, ChangesSummary)> {
        let sessions = self.sessions.lock();
        let entry = sessions.get(session_id)?;
        if !entry.has_repo {
            return None;
        }
        let mut additions = 0i64;
        let mut deletions = 0i64;
        for file in &entry.files {
            if let Some(diff) = &file.edit.diff {
                additions += diff.added.unwrap_or(0);
                deletions += diff.removed.unwrap_or(0);
            }
        }
        let summary = ChangesSummary {
            additions: Some(additions),
            deletions: Some(deletions),
            files: Some(entry.files.len() as i64),
        };
        Some((vec![catalogue_entry(session_id)], summary))
    }

    /// Drop one session's cached changeset (its directories and full patch
    /// bodies) — the dispose path; without it the table only ever grows on a
    /// long-running host.
    pub fn forget(&self, session_id: &str) {
        self.sessions.lock().remove(session_id);
    }

    /// The full changeset state, answered from the last scan. Unknown
    /// session or key → `None` (the host answers `NotFound`).
    pub fn state(&self, session_id: &str, key: &str) -> Option<ChangesetState> {
        if key != KEY {
            return None;
        }
        let sessions = self.sessions.lock();
        let entry = sessions.get(session_id)?;
        // A session with no repository never advertised the channel; its
        // state answers NotFound like any other absent resource — "absent",
        // not "empty", is the honest shape (same wording as the catalogue).
        if !entry.has_repo {
            return None;
        }
        // The review set is live authority: overlay it so a state read
        // between the toggle and the next recompute still shows the flags.
        let files = entry
            .files
            .iter()
            .map(|file| ChangesetFile {
                reviewed: entry.reviewed.contains(&file.id).then_some(true),
                ..file.clone()
            })
            .collect();
        Some(ChangesetState {
            status: entry.status.clone(),
            error: entry.error.clone(),
            files,
            operations: Some(vec![revert_operation()]),
        })
    }

    /// Record a reviewer's flags. Unknown file ids are ignored (the reducer
    /// does the same); an unknown changeset is a refusal — a client toggling
    /// review on a changeset that does not exist must not be told it landed.
    pub fn review(
        &self,
        session_id: &str,
        key: &str,
        file_ids: &[String],
        reviewed: bool,
    ) -> Result<(), String> {
        if key != KEY {
            return Err(format!("unknown changeset: {key}"));
        }
        let mut sessions = self.sessions.lock();
        let entry = sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("no changeset for session {session_id}"))?;
        if reviewed {
            entry.reviewed.extend(file_ids.iter().cloned());
        } else {
            for id in file_ids {
                entry.reviewed.remove(id);
            }
        }
        Ok(())
    }

    /// Re-scan and emit the replacement actions: `contentChanged` (files with
    /// the operation list; review flags carried over and cleared where the
    /// patch changed, per the spec's reset rule) then the settled
    /// `statusChanged`. No transient `recomputing` edge is emitted here — the
    /// scan is synchronous, so subscribers only ever see the settled edges;
    /// the `catalogue()` re-seed is where a dirs change marks an existing
    /// entry `Recomputing`. Empty when the session has no changeset.
    pub fn recompute(&self, session_id: &str) -> Vec<(String, StateAction)> {
        let scan = {
            let sessions = self.sessions.lock();
            let Some(entry) = sessions.get(session_id) else {
                return Vec::new();
            };
            scan(&*self.git, &entry.dirs)
        };
        let channel = uri(session_id);
        let mut sessions = self.sessions.lock();
        let Some(entry) = sessions.get_mut(session_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match scan {
            Ok(scan) => {
                entry.has_repo = scan.repos > 0;
                // Previous patch *presence*, not just content: a deletion
                // entry has no patch at all, and comparing it against a
                // missing map slot would reset its review on every recompute
                // even though nothing about it changed.
                let previous_patches: HashMap<&str, Option<&str>> = entry
                    .files
                    .iter()
                    .map(|file| {
                        (
                            file.id.as_str(),
                            file.meta
                                .as_ref()
                                .and_then(|meta| meta.get("patch"))
                                .and_then(Value::as_str),
                        )
                    })
                    .collect();
                entry.files = scan
                    .files
                    .iter()
                    .map(|raw| {
                        // Compare the *wire* presence: an empty patch body is
                        // omitted on the wire, so a deletion (always
                        // patch-less) compares equal to its previous self.
                        let new_patch = (!raw.patch.is_empty()).then_some(raw.patch.as_str());
                        let unchanged = previous_patches.get(raw.id.as_str()) == Some(&new_patch);
                        let reviewed = if unchanged && entry.reviewed.contains(raw.id.as_str()) {
                            Some(true)
                        } else {
                            None
                        };
                        if !unchanged {
                            entry.reviewed.remove(raw.id.as_str());
                        }
                        wire_file(raw, reviewed)
                    })
                    .collect();
                entry.status = ChangesetStatus::Ready;
                entry.error = None;
                out.push((
                    channel.clone(),
                    StateAction::ChangesetContentChanged(Box::new(ChangesetContentChangedAction {
                        files: entry.files.clone(),
                        operations: Some(vec![revert_operation()]),
                    })),
                ));
                out.push((
                    channel,
                    StateAction::ChangesetStatusChanged(ChangesetStatusChangedAction {
                        status: ChangesetStatus::Ready,
                        error: None,
                    }),
                ));
            }
            Err(message) => {
                entry.status = ChangesetStatus::Error;
                entry.error = Some(ErrorInfo {
                    error_type: "changeset".to_string(),
                    message,
                    stack: None,
                    meta: None,
                });
                out.push((
                    channel,
                    StateAction::ChangesetStatusChanged(ChangesetStatusChangedAction {
                        status: ChangesetStatus::Error,
                        error: entry.error.clone(),
                    }),
                ));
            }
        }
        out
    }

    /// Execute the `revert` operation: restore the selected paths to HEAD
    /// (untracked files are deleted). Returns the result message; the caller
    /// then runs [`Engine::recompute`] and publishes its emissions.
    pub fn invoke_revert(
        &self,
        session_id: &str,
        key: &str,
        target: Option<&ahp_types::commands::ChangesetOperationTarget>,
    ) -> Result<String, String> {
        if key != KEY {
            return Err(format!("unknown changeset: {key}"));
        }
        let paths: Vec<String> = {
            let sessions = self.sessions.lock();
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| format!("no changeset for session {session_id}"))?;
            match target {
                None => entry.files.iter().map(|file| file.id.clone()).collect(),
                Some(ahp_types::commands::ChangesetOperationTarget::Resource {
                    resource, ..
                }) => {
                    let id = resource.as_str();
                    if !entry.files.iter().any(|file| file.id == id) {
                        return Err(format!("target {id} is not in the changeset"));
                    }
                    vec![id.to_string()]
                }
                Some(ahp_types::commands::ChangesetOperationTarget::Range { .. }) => {
                    return Err("revert does not accept a range target".to_string());
                }
                Some(ahp_types::commands::ChangesetOperationTarget::Unknown(value)) => {
                    return Err(format!("unknown target: {value}"));
                }
            }
        };
        if paths.is_empty() {
            return Ok("Nothing to revert — the changeset is empty.".to_string());
        }
        self.restore(session_id, &paths)?;
        Ok(format!(
            "Reverted {} path{} back to HEAD.",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ))
    }

    /// Restore paths to HEAD in whichever scanned repo contains them;
    /// untracked files are deleted outright (`checkout HEAD` cannot).
    fn restore(&self, session_id: &str, paths: &[String]) -> Result<(), String> {
        let dirs: Vec<PathBuf> = {
            let sessions = self.sessions.lock();
            sessions
                .get(session_id)
                .map(|entry| entry.dirs.clone())
                .unwrap_or_default()
        };
        let mut failures = Vec::new();
        for id in paths {
            // The wire id is `file://` + the absolute worktree path; git and
            // the filesystem want the bare path.
            let Some(path) = id.strip_prefix("file://") else {
                failures.push(format!("{id}: not a file URI"));
                continue;
            };
            let abs = PathBuf::from(path);
            // The most specific (longest) matching directory wins when grants
            // nest — `/repo` and `/repo/sub` both contain `/repo/sub/f.rs`,
            // and only the inner one gives a sane relative path.
            let Some(root) = dirs
                .iter()
                .filter(|dir| abs.starts_with(dir))
                .max_by_key(|dir| dir.components().count())
            else {
                failures.push(format!("{path}: outside the session's directories"));
                continue;
            };
            // "In HEAD" — not "in the index": an index-only new file
            // (`git add` without a commit) passes ls-files but has no HEAD
            // side, and `checkout HEAD --` fails on it. `HEAD:<path>` is
            // *repo-root*-relative while `rel` is relative to the granted
            // directory (often a monorepo subdirectory) — the `./` form is
            // cwd-relative, which is what the path actually is.
            let Ok(rel) = abs
                .strip_prefix(root)
                .map(|rest| rest.to_string_lossy().into_owned())
            else {
                failures.push(format!("{path}: not under its repo root"));
                continue;
            };
            let in_head = self
                .git
                .run(root, &["cat-file", "-e", &format!("HEAD:./{rel}")])
                .map(|out| out.success)
                .unwrap_or(false);
            if in_head {
                if let Err(e) = self.git.stdout(root, &["checkout", "HEAD", "--", &rel]) {
                    failures.push(format!("{path}: {e}"));
                }
            } else {
                // New relative to HEAD: unstage it first when it was added
                // to the index (a no-op for plain untracked files), then
                // delete the worktree copy — reverting a new file means it
                // should not exist.
                let _ = self.git.run(root, &["rm", "-f", "--cached", "--", &rel]);
                if let Err(e) = std::fs::remove_file(&abs) {
                    failures.push(format!("{path}: {e}"));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A git runner driven by a closure — each test scripts exactly the
    /// invocations its scenario needs, matched on the leading args.
    type GitScript = Box<dyn Fn(&[&str]) -> Result<GitOutput, String> + Send + Sync>;

    struct FakeGit(GitScript);

    impl GitRunner for FakeGit {
        fn run(&self, _cwd: &Path, args: &[&str]) -> Result<GitOutput, String> {
            (self.0)(args)
        }
    }

    fn ok(stdout: &str) -> Result<GitOutput, String> {
        Ok(GitOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn script(f: impl Fn(&[&str]) -> Result<GitOutput, String> + Send + Sync + 'static) -> Engine {
        Engine::with_git(Arc::new(FakeGit(Box::new(f))))
    }

    const A_PATCH: &str = "--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-old\n+new\n";

    /// One modified tracked file `f.rs` (3 added / 1 removed), inside a repo.
    fn one_file_git() -> Engine {
        script(move |args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok("/repo\n"),
            "status" => ok(" M f.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("3\t1\tf.rs\n"),
            "diff" => ok(A_PATCH),
            "ls-files" => ok("f.rs\n"),
            "checkout" => ok(""),
            _ => ok(""),
        })
    }

    #[test]
    fn uri_and_parse_round_trip() {
        assert_eq!(parse(&uri("s-1")), Some(("s-1".to_string(), KEY)));
        assert_eq!(parse("ahp-changeset:/s-1"), None);
        assert_eq!(parse("ahp-chat:/c-1"), None);
    }

    #[test]
    fn chat_face_counts_the_footprint_over_the_last_scan() {
        let engine = one_file_git();
        engine
            .catalogue("s-1", vec![PathBuf::from("/repo")])
            .expect("repo present");
        let (changesets, changes) = engine.chat_face("s-1").expect("the face follows the scan");
        assert_eq!(changesets.len(), 1);
        assert_eq!(changesets[0].label, "Uncommitted Changes");
        assert_eq!(changes.additions, Some(3));
        assert_eq!(changes.deletions, Some(1));
        assert_eq!(changes.files, Some(1));
        // A session the engine has never seen has no face (the never-advertised
        // shape; the no-repo case is covered by
        // `a_session_with_no_repository_advertises_nothing`).
        assert!(engine.chat_face("s-unknown").is_none());
    }

    #[test]
    fn a_repo_session_advertises_the_catalogue_and_answers_a_ready_state() {
        let engine = one_file_git();
        // The grant IS the repo root here (the dir == repo-root case); the
        // subdirectory-grant case has its own dedicated tests below.
        let dirs = vec![PathBuf::from("/repo")];
        let entry = engine.catalogue("s-1", dirs.clone()).expect("repo present");
        assert_eq!(entry.label, "Uncommitted Changes");
        assert_eq!(entry.change_kind, KEY);
        assert_eq!(entry.uri_template, uri("s-1"));
        let capabilities = entry.capabilities.expect("review advertised");
        assert!(capabilities.review.is_some(), "the review workflow is on");

        let state = engine.state("s-1", KEY).expect("state exists");
        assert_eq!(state.status, ChangesetStatus::Ready);
        assert_eq!(state.files.len(), 1);
        let file = &state.files[0];
        assert_eq!(file.id, "file:///repo/f.rs");
        let after = file
            .edit
            .after
            .as_ref()
            .expect("live file has an after side");
        assert_eq!(after.uri, "file:///repo/f.rs");
        let diff = file.edit.diff.as_ref().expect("diff carries the stats");
        assert_eq!(diff.added, Some(3));
        assert_eq!(diff.removed, Some(1));
        assert_eq!(
            file.meta.as_ref().and_then(|meta| meta.get("patch")),
            Some(&Value::String(A_PATCH.to_string()))
        );
        assert_eq!(file.reviewed, None);
    }

    #[test]
    fn a_session_with_no_repository_advertises_nothing() {
        let engine = script(|args| match args.first().copied().unwrap_or("") {
            "rev-parse" => Err("not a git repository".to_string()),
            _ => ok(""),
        });
        assert!(
            engine
                .catalogue("s-1", vec![PathBuf::from("/plain")])
                .is_none()
        );
        // And the channel state answers *absent* — "no repository here" is
        // the same shape as "no such resource", never an empty changeset.
        assert!(engine.state("s-1", KEY).is_none());
    }

    #[test]
    fn an_unknown_key_is_not_served() {
        let engine = one_file_git();
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        assert!(engine.state("s-1", "turn").is_none());
        assert!(engine.review("s-1", "turn", &[], true).is_err());
    }

    #[test]
    fn review_carries_over_unchanged_patches_and_resets_changed_ones() {
        // One engine, two patch generations: the flip is what a recompute
        // after more agent edits looks like.
        let generation = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let generation_clone = Arc::clone(&generation);
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok("/repo\n"),
            "status" => ok(" M f.rs\0"),
            "diff" if args.contains(&"--numstat") => {
                if generation_clone.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    ok("3\t1\tf.rs\n")
                } else {
                    ok("9\t0\tf.rs\n")
                }
            }
            "diff" if !args.contains(&"--numstat") => {
                if generation_clone.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    ok(A_PATCH)
                } else {
                    ok("--- a/f.rs\n+++ b/f.rs\n@@ -1,9 +1 @@\n")
                }
            }
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        engine
            .review("s-1", KEY, &["file:///repo/f.rs".to_string()], true)
            .expect("the file exists");
        // Same patch → the flag rides along, in the state read directly.
        let state = engine.state("s-1", KEY).unwrap();
        assert_eq!(state.files[0].reviewed, Some(true));
        // The patch changes under the stable id → the engine resets review
        // explicitly (the spec's rule: no content version, so the server is
        // the reset authority).
        generation.store(1, std::sync::atomic::Ordering::Relaxed);
        let mut reset = None;
        for (channel, action) in engine.recompute("s-1") {
            assert_eq!(channel, uri("s-1"));
            if let StateAction::ChangesetContentChanged(content) = action {
                reset = Some(content.files[0].reviewed);
            }
        }
        assert_eq!(reset, Some(None), "a changed patch must reset review");
        // And a recompute over an unchanged patch keeps the flag.
        engine
            .review("s-1", KEY, &["file:///repo/f.rs".to_string()], true)
            .unwrap();
        let state = engine.state("s-1", KEY).unwrap();
        assert_eq!(state.files[0].reviewed, Some(true));
    }

    #[test]
    fn recompute_emits_content_then_status_on_the_changeset_channel() {
        let engine = one_file_git();
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        let emissions = engine.recompute("s-1");
        assert_eq!(emissions.len(), 2);
        assert!(matches!(
            emissions[0].1,
            StateAction::ChangesetContentChanged(_)
        ));
        assert!(matches!(
            &emissions[1].1,
            StateAction::ChangesetStatusChanged(status) if status.status == ChangesetStatus::Ready
        ));
    }

    #[test]
    fn revert_restores_tracked_paths_and_deletes_untracked_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("workdir");
        let untracked = work.join("scratch.txt");
        std::fs::write(&untracked, "delete me").expect("untracked file");

        // git resolves symlinks, so the fake root must be the canonical
        // shape — the engine canonicalizes the granted dirs to match.
        let root_display = work.canonicalize().unwrap().display().to_string();
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            // The fake repo root IS the real tempdir, so the untracked
            // revert's delete lands on a real file.
            "rev-parse" => ok(&format!("{root_display}\n")),
            "status" => ok(" M f.rs\0?? scratch.txt\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t1\tf.rs\n"),
            "diff" if args.contains(&"/dev/null") => ok("+delete me\n"),
            "diff" => ok(A_PATCH),
            // `cat-file -e HEAD:<path>` decides the restore branch: the
            // tracked file is in HEAD, the untracked one is not.
            "cat-file" if args.iter().any(|a| a.contains("scratch")) => {
                Err("no HEAD side".to_string())
            }
            "cat-file" => ok(""),
            "ls-files" if args.iter().any(|a| a.contains("scratch")) => {
                Err("untracked".to_string())
            }
            "ls-files" => ok("f.rs\n"),
            "checkout" => ok(""),
            "rm" => ok(""),
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![work.clone()]);
        // The untracked file's id is the engine's absolute path; retarget the
        // revert through the engine's own view by asking for the whole set.
        let state = engine.state("s-1", KEY).unwrap();
        let ids: Vec<String> = state.files.iter().map(|f| f.id.clone()).collect();
        assert!(ids.iter().any(|id| id.ends_with("scratch.txt")));
        // Resource-scoped revert of the untracked file actually deletes it.
        let target = ahp_types::commands::ChangesetOperationTarget::Resource {
            resource: ids
                .iter()
                .find(|id| id.ends_with("scratch.txt"))
                .unwrap()
                .clone(),
            side: None,
        };
        engine
            .invoke_revert("s-1", KEY, Some(&target))
            .expect("revert lands");
        assert!(!untracked.exists(), "an untracked revert is a delete");
        // Changeset-scope validation: a foreign id is refused.
        let foreign = ahp_types::commands::ChangesetOperationTarget::Resource {
            resource: "file:///elsewhere/x.rs".to_string(),
            side: None,
        };
        assert!(engine.invoke_revert("s-1", KEY, Some(&foreign)).is_err());
        // Range targets are declared out of scope.
        let range = ahp_types::commands::ChangesetOperationTarget::Range {
            resource: ids[0].clone(),
            side: None,
            range: ahp_types::state::TextRange {
                start: ahp_types::state::TextPosition {
                    line: 0,
                    character: 0,
                },
                end: ahp_types::state::TextPosition {
                    line: 1,
                    character: 0,
                },
            },
        };
        assert!(engine.invoke_revert("s-1", KEY, Some(&range)).is_err());
    }

    #[test]
    fn revert_restores_an_index_only_new_file_by_unstaging_then_deleting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("workdir");
        let added = work.join("added.txt");
        std::fs::write(&added, "staged but never committed").expect("added file");
        let root_display = work.canonicalize().unwrap().display().to_string();
        let checkouts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let checkouts_clone = Arc::clone(&checkouts);
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            // The fake repo root IS the real tempdir, so the worktree delete
            // lands on a real file.
            "rev-parse" => ok(&format!("{root_display}\n")),
            "status" => ok("A  added.txt\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t0\tadded.txt\n"),
            "diff" => ok("+staged but never committed\n"),
            // `cat-file -e HEAD:added.txt` fails: the file has no HEAD side —
            // exactly the index-only case `ls-files` could not distinguish.
            "cat-file" => Err("path does not exist in HEAD".to_string()),
            "rm" => ok(""),
            "checkout" => {
                checkouts_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                ok("")
            }
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![work.clone()]);
        // The engine's ids carry the canonical (git-resolved) path.
        let target = ahp_types::commands::ChangesetOperationTarget::Resource {
            resource: format!("file://{}", added.canonicalize().unwrap().display()),
            side: None,
        };
        engine
            .invoke_revert("s-1", KEY, Some(&target))
            .expect("the index-only revert lands");
        assert!(!added.exists(), "reverting a new file deletes it");
        assert_eq!(
            checkouts.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "checkout HEAD would fail on an index-only path; the restore must take the unstage+delete branch"
        );
    }

    #[test]
    fn a_copied_file_does_not_list_its_source_as_deleted() {
        let engine = script(|args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok("/repo\n"),
            // `C ` carries two NUL fields: the new path, then the source.
            "status" => ok("C  copy.rs\0original.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("5\t0\tcopy.rs\n"),
            "diff" => ok("+copy body\n"),
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        let state = engine.state("s-1", KEY).unwrap();
        let ids: Vec<&str> = state.files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["file:///repo/copy.rs"],
            "the copy's source still exists in the worktree — it is not a deletion"
        );
    }

    #[test]
    fn a_deleted_file_keeps_its_review_flag_across_recomputes() {
        // A deletion entry carries no patch at all; "no patch" is its stable
        // content, so the review flag must survive (the old patch-string
        // comparison read the missing patch as "changed" and reset it every
        // turn).
        let engine = script(|args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok("/repo\n"),
            "status" => ok(" D f.rs\0"),
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        engine
            .review("s-1", KEY, &["file:///repo/f.rs".to_string()], true)
            .expect("the deletion entry exists");
        let mut carried = None;
        for (_, action) in engine.recompute("s-1") {
            if let StateAction::ChangesetContentChanged(content) = action {
                carried = Some(content.files[0].reviewed);
            }
        }
        assert_eq!(
            carried,
            Some(Some(true)),
            "an unchanged deletion keeps its review flag"
        );
    }

    #[test]
    fn a_tracked_file_under_a_subdirectory_grant_is_restored_not_deleted() {
        // The session's granted directory sits INSIDE the repo (monorepo
        // shape). `HEAD:<path>` is repo-root-relative, so the restore must
        // use the cwd-relative `HEAD:./<path>` — the root-relative form
        // misreads "in HEAD" and the "new file" branch would delete the
        // user's modified tracked file.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).expect("subdir");
        let tracked = sub.join("f.rs");
        std::fs::write(&tracked, "locally modified").expect("tracked file");
        let repo_canon = repo
            .canonicalize()
            .expect("canonical repo")
            .display()
            .to_string();
        let grant = sub.canonicalize().expect("canonical grant");
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            // git resolves the symlink-free real root, even when invoked
            // from inside the granted subdirectory.
            "rev-parse" => ok(&format!("{repo_canon}\n")),
            "status" => ok(" M sub/f.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t1\tsub/f.rs\n"),
            "diff" => ok("--- a/sub/f.rs\n+++ b/sub/f.rs\n@@ -1 +1 @@\n-old\n+new\n"),
            // The cwd-relative form finds it; the root-relative form (the
            // bug) would not — script exactly that distinction.
            "cat-file" if args.iter().any(|a| a.starts_with("HEAD:./")) => ok(""),
            "cat-file" => {
                Err("repo-root-relative path does not resolve from the subdir".to_string())
            }
            "checkout" => ok(""),
            "rm" => {
                panic!("the unstage branch must never run for a HEAD-tracked file");
            }
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![grant]);
        let target = ahp_types::commands::ChangesetOperationTarget::Resource {
            resource: format!("file://{}", tracked.canonicalize().unwrap().display()),
            side: None,
        };
        engine
            .invoke_revert("s-1", KEY, Some(&target))
            .expect("the subdir revert lands");
        assert!(
            tracked.exists(),
            "a restored tracked file must survive its own revert"
        );
    }

    #[test]
    fn a_grant_recorded_through_a_symlink_still_restores() {
        // macOS /tmp-style shapes: the session recorded a symlinked path,
        // git reports the resolved one. Canonicalization at entry time puts
        // both sides on the same shape; without it every revert would be
        // refused with "outside the session's directories".
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).expect("subdir");
        let tracked = sub.join("f.rs");
        std::fs::write(&tracked, "content").expect("tracked file");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo, &link).expect("symlink");
        let repo_canon = repo
            .canonicalize()
            .expect("canonical repo")
            .display()
            .to_string();
        let grant = link.join("sub");
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok(&format!("{repo_canon}\n")),
            "status" => ok(" M sub/f.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t0\tsub/f.rs\n"),
            "diff" => ok("--- a/sub/f.rs\n+++ b/sub/f.rs\n@@ -1 +1 @@\n-old\n+new\n"),
            "cat-file" if args.iter().any(|a| a.starts_with("HEAD:./")) => ok(""),
            "cat-file" => Err("not in HEAD".to_string()),
            "checkout" => ok(""),
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![grant]);
        let target = ahp_types::commands::ChangesetOperationTarget::Resource {
            resource: format!("file://{}", tracked.canonicalize().unwrap().display()),
            side: None,
        };
        engine
            .invoke_revert("s-1", KEY, Some(&target))
            .expect("the symlinked grant resolves");
    }

    #[test]
    fn a_repository_appearing_after_the_first_scan_is_picked_up() {
        // Session starts outside any repo; cwd later moves into one. The
        // entry already exists, so the catalogue must notice the directory
        // set moved and rescan — otherwise the session never advertises.
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scans_clone = Arc::clone(&scans);
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            "rev-parse" => {
                if scans_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                    Err("not a git repository".to_string())
                } else {
                    ok("/repo\n")
                }
            }
            "status" => ok(" M f.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t1\tf.rs\n"),
            "diff" => ok("+x\n"),
            _ => ok(""),
        });
        assert!(
            engine
                .catalogue("s-1", vec![PathBuf::from("/plain")])
                .is_none()
        );
        let entry = engine
            .catalogue("s-1", vec![PathBuf::from("/repo")])
            .expect("the repo appears on the moved directory set");
        assert_eq!(entry.uri_template, uri("s-1"));
        assert_eq!(engine.state("s-1", KEY).unwrap().files.len(), 1);
    }

    #[test]
    fn a_dirs_move_rescans_under_recomputing_with_the_previous_files_intact() {
        // A moved directory set on an existing entry is `Recomputing`, not a
        // first `Computing`: while the moved-set scan runs, a state read
        // reports the transient status and still serves the previous
        // completed result, then settles `Ready` on the new set.
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scans_clone = Arc::clone(&scans);
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started_clone = Arc::clone(&started);
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_clone = Arc::clone(&release);
        let engine = Arc::new(script(move |args| {
            match args.first().copied().unwrap_or("") {
                "rev-parse" => {
                    if scans_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                        ok("/repo\n")
                    } else {
                        started_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                        while !release_clone.load(std::sync::atomic::Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        ok("/repo2\n")
                    }
                }
                "status" => ok(" M f.rs\0"),
                "diff" if args.contains(&"--numstat") => ok("1\t1\tf.rs\n"),
                "diff" => ok("+x\n"),
                _ => ok(""),
            }
        }));
        engine.catalogue("s-1", vec![PathBuf::from("/repo")]);
        assert_eq!(
            engine.state("s-1", KEY).unwrap().status,
            ChangesetStatus::Ready
        );

        let mover = Arc::clone(&engine);
        let handle =
            std::thread::spawn(move || mover.catalogue("s-1", vec![PathBuf::from("/repo2")]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !started.load(std::sync::atomic::Ordering::Relaxed) {
            assert!(
                std::time::Instant::now() < deadline,
                "the moved-set scan never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mid = engine
            .state("s-1", KEY)
            .expect("the previous completed result still serves");
        assert_eq!(mid.status, ChangesetStatus::Recomputing);
        assert_eq!(
            mid.files.len(),
            1,
            "the previous completed file list stays in place mid-rescan"
        );
        assert_eq!(mid.files[0].id, "file:///repo/f.rs");

        release.store(true, std::sync::atomic::Ordering::Relaxed);
        handle
            .join()
            .unwrap()
            .expect("the moved set holds a repository");
        let settled = engine.state("s-1", KEY).unwrap();
        assert_eq!(settled.status, ChangesetStatus::Ready);
        assert_eq!(settled.files[0].id, "file:///repo2/f.rs");
    }

    #[test]
    fn a_monorepo_subdirectory_grant_never_lists_out_of_grant_files() {
        // Probe-D shape: the grant is /repo/sub but the repo also carries
        // /repo/other/secret.rs. git scans whole repos; the fence filters
        // the view — the out-of-grant file must not be listed (its patch
        // would cross the resource fence, and revert could never act on it).
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let sub = repo.join("sub");
        let other = repo.join("other");
        std::fs::create_dir_all(&sub).expect("subdir");
        std::fs::create_dir_all(&other).expect("otherdir");
        let repo_canon = repo
            .canonicalize()
            .expect("canonical repo")
            .display()
            .to_string();
        let engine = script(move |args| match args.first().copied().unwrap_or("") {
            "rev-parse" => ok(&format!("{repo_canon}\n")),
            "status" => ok(" M sub/f.rs\0 M other/secret.rs\0"),
            "diff" if args.contains(&"--numstat") => ok("1\t1\tsub/f.rs\n"),
            "diff" => ok("+sub change\n"),
            _ => ok(""),
        });
        engine.catalogue("s-1", vec![sub.canonicalize().unwrap()]);
        let state = engine.state("s-1", KEY).unwrap();
        let ids: Vec<&str> = state.files.iter().map(|f| f.id.as_str()).collect();
        // The assertion compares against the CANONICAL grant path: the wire
        // id is built from the git-reported (resolved) repo root.
        let sub_canon = sub.canonicalize().unwrap();
        assert_eq!(
            ids,
            vec![format!("file://{}", sub_canon.join("f.rs").display())],
            "only the in-grant file is listed; the out-of-grant change is fenced out"
        );
    }

    #[test]
    fn binary_files_count_as_zero_lines() {
        assert_eq!(parse_numstat("-\t-\tasset.png\n"), (0, 0));
        assert_eq!(parse_numstat("12\t3\tf.rs\n"), (12, 3));
        assert_eq!(parse_numstat(""), (0, 0));
    }

    #[test]
    fn oversized_patches_are_truncated_with_a_note() {
        let big = "x".repeat(PATCH_CAP + 1);
        let cut = truncate_patch(big);
        assert!(cut.len() < PATCH_CAP + 20);
        assert!(cut.ends_with("… (truncated)"));
    }
}
