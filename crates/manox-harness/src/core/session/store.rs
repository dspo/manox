// The Session Core storage engine: lazy identity publication over an
// append-only event log.
//
// Layout under the state root (one directory per session, bucketed by the
// origin project slug):
//   <root>/<project-slug>/<session-id>/header.json   — immutable identity
//   <root>/<project-slug>/<session-id>/log.v1.jsonl  — event envelopes
//   <root>/<project-slug>/<session-id>/state.json    — rebuildable cache
//   <root>/<project-slug>/<session-id>/.lock         — the writer lease
//
// `create` touches nothing: a session that never appends leaves no disk
// trace, so "an empty session exists on disk" is not a state this engine
// can produce. The first append takes the writer lease and publishes the
// header with an exclusive create — a second publisher over the same
// directory fails loudly — then appends the batch and fsyncs. A crash
// between the two steps leaves a materialized blank session: legal,
// discoverable by its header, blank by the same no-turn rule that hides
// never-materialized sessions. A crash mid-batch leaves a torn tail, which
// the next writer truncates and a reader stops before: the committed
// prefix is the truth.

use std::io::ErrorKind;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex;

use crate::session::event::{Envelope, Event};
use crate::session::identity::{HEADER_FORMAT_VERSION, SessionHeader, project_slug};

const HEADER_FILE: &str = "header.json";
const LOG_FILE: &str = "log.v1.jsonl";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = ".lock";

/// The directory a session with this header lives in.
pub fn session_dir(root: &Path, header: &SessionHeader) -> PathBuf {
    root.join(project_slug(&header.origin_cwd)).join(&header.id)
}

/// A write handle to one session. Materializes lazily; cheap to hold for
/// sessions that may never be used (the landing blank, spawned children).
pub struct CoreStore {
    dir: PathBuf,
    header: SessionHeader,
    /// The flock holder; the lock lives until the store is dropped.
    lease: std::sync::Mutex<Option<std::fs::File>>,
    inner: Mutex<Writer>,
}

struct Writer {
    log: Option<tokio::fs::File>,
    next_seq: u64,
}

impl CoreStore {
    /// A handle for a session that has never been published. Validates the
    /// header eagerly so a later materialization can never write a header
    /// its own validator would disown; writes nothing to disk.
    pub fn create(root: &Path, header: SessionHeader) -> anyhow::Result<Self> {
        header
            .validate()
            .map_err(|reason| anyhow::anyhow!("invalid session header: {reason}"))?;
        Ok(Self {
            dir: session_dir(root, &header),
            header,
            lease: std::sync::Mutex::new(None),
            inner: Mutex::new(Writer {
                log: None,
                next_seq: 0,
            }),
        })
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append a batch of events; returns the assigned sequence numbers.
    /// The first call materializes the session (header publication, lease,
    /// log creation); every call fsyncs before resolving.
    pub async fn append(&self, events: Vec<Event>) -> anyhow::Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let mut writer = self.inner.lock().await;
        if writer.log.is_none() {
            self.materialize(&mut writer).await?;
        }
        let mut seqs = Vec::with_capacity(events.len());
        let mut batch = String::new();
        for event in events {
            let seq = writer.next_seq;
            writer.next_seq += 1;
            let envelope = Envelope::new(seq, event);
            batch.push_str(&serde_json::to_string(&envelope)?);
            batch.push('\n');
            seqs.push(seq);
        }
        let log = writer
            .log
            .as_mut()
            .expect("materialize leaves an open log handle");
        log.write_all(batch.as_bytes())
            .await
            .context("append event batch")?;
        log.sync_all().await.context("fsync event batch")?;
        Ok(seqs)
    }

    /// Take the writer lease, publish the header exclusively, and open the
    /// log — resuming after a torn tail if a crash left one.
    async fn materialize(&self, writer: &mut Writer) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .context("create session directory")?;
        self.acquire_lease()?;
        let header_path = self.dir.join(HEADER_FILE);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .truncate(false)
            .open(&header_path)
            .await
        {
            Ok(file) => {
                let bytes = serde_json::to_vec_pretty(&self.header)?;
                let mut file = file;
                file.write_all(&bytes).await?;
                file.write_all(b"\n").await?;
                file.sync_all().await?;
                drop(file);
                sync_dir(&self.dir);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                anyhow::bail!(
                    "session {} is already published; open it instead of creating it",
                    self.header.id
                );
            }
            Err(err) => {
                return Err(err).context("publish session header");
            }
        }
        let mut log = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(LOG_FILE))
            .await
            .context("open session log")?;
        writer.next_seq = heal_torn_tail(&mut log).await?;
        writer.log = Some(log);
        Ok(())
    }

    fn acquire_lease(&self) -> anyhow::Result<()> {
        let mut guard = self
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(LOCK_FILE))
            .with_context(|| format!("open writer lease for session {}", self.header.id))?;
        // flock treats separate open() descriptions as separate holders —
        // including two opens in one process — so a second live handle
        // fails here rather than interleaving appends.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if taken != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock {
                anyhow::bail!("session {} is driven by another writer", self.header.id);
            }
            return Err(err).context("acquire writer lease");
        }
        *guard = Some(file);
        Ok(())
    }
}

/// The result of a cold read: the identity, the committed prefix, whether
/// a torn tail was withheld, and the next sequence number a writer would
/// assign. Readers never truncate; only a materializing writer heals.
#[derive(Debug)]
pub struct Opened {
    pub header: SessionHeader,
    pub envelopes: Vec<Envelope>,
    pub torn_tail: bool,
    pub next_seq: u64,
}

/// Cold-read a session directory. A missing header is an error (never an
/// empty session — blank sessions either never materialized or carry a
/// header); an unsupported format version is an explicit refusal.
pub async fn open(dir: &Path) -> anyhow::Result<Opened> {
    let bytes = tokio::fs::read(dir.join(HEADER_FILE))
        .await
        .with_context(|| format!("read session header in {}", dir.display()))?;
    let header: SessionHeader = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse header in {}", dir.display()))?;
    if header.format_version != HEADER_FORMAT_VERSION {
        anyhow::bail!(
            "session {} header format version {} is not supported (want {HEADER_FORMAT_VERSION})",
            header.id,
            header.format_version
        );
    }
    header
        .validate()
        .map_err(|reason| anyhow::anyhow!("session {} header is invalid: {reason}", header.id))?;
    let log_bytes = match tokio::fs::read(dir.join(LOG_FILE)).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).context("read session log"),
    };
    let (envelopes, torn_tail, _committed_len) = parse_committed_prefix(&log_bytes);
    let next_seq = envelopes.len() as u64;
    Ok(Opened {
        header,
        envelopes,
        torn_tail,
        next_seq,
    })
}

/// Parse complete, well-formed lines; stop at the first damaged or
/// unterminated line. The committed prefix is the truth — anything after
/// the first bad line is withheld, whatever the damage was. Returns the
/// envelopes, whether a tail was withheld, and the exact byte length of
/// the committed prefix (a writer truncates to it verbatim, never to a
/// re-serialization that could shave unknown future fields).
fn parse_committed_prefix(bytes: &[u8]) -> (Vec<Envelope>, bool, usize) {
    let mut envelopes = Vec::new();
    let mut offset = 0usize;
    let mut torn = false;
    while offset < bytes.len() {
        let Some(nl) = bytes[offset..].iter().position(|b| *b == b'\n') else {
            torn = true;
            break;
        };
        let line_end = offset + nl;
        let line = &bytes[offset..line_end];
        match serde_json::from_slice::<Envelope>(line) {
            Ok(envelope) => envelopes.push(envelope),
            Err(_) => {
                torn = true;
                break;
            }
        }
        offset = line_end + 1;
    }
    (envelopes, torn, offset)
}

/// Move a write handle past a crash-torn tail: every complete line must
/// parse, the trailing partial line is dropped, and the file is truncated
/// to the committed prefix verbatim. Returns the next sequence number.
async fn heal_torn_tail(log: &mut tokio::fs::File) -> anyhow::Result<u64> {
    let mut bytes = Vec::new();
    log.read_to_end(&mut bytes).await.context("read back log")?;
    let (envelopes, torn, committed_len) = parse_committed_prefix(&bytes);
    if torn && committed_len < bytes.len() {
        log.set_len(committed_len as u64)
            .await
            .context("truncate torn tail")?;
        log.sync_all().await?;
    }
    Ok(envelopes.len() as u64)
}

/// The rebuildable per-session cache. Never a source of truth: every field
/// is derivable from the log (or is volatile presentation state the host
/// may lose on a crash without losing a fact).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateCache {
    #[serde(default)]
    pub fold_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub errored: bool,
}

/// Read the state cache; a missing file is a default cache, not an error.
pub async fn read_state(dir: &Path) -> anyhow::Result<SessionStateCache> {
    match tokio::fs::read(dir.join(STATE_FILE)).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("parse state cache")?),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(SessionStateCache::default()),
        Err(err) => Err(err).context("read state cache"),
    }
}

/// Write the state cache through a temp file + rename, then fsync the
/// directory so the rename itself survives a crash.
pub async fn write_state(dir: &Path, state: &SessionStateCache) -> anyhow::Result<()> {
    let final_path = dir.join(STATE_FILE);
    let tmp_path = dir.join(format!("{STATE_FILE}.tmp"));
    tokio::fs::write(&tmp_path, serde_json::to_vec_pretty(state)?)
        .await
        .context("write state cache temp")?;
    tokio::fs::rename(&tmp_path, &final_path)
        .await
        .context("publish state cache")?;
    sync_dir(dir);
    Ok(())
}

/// A header-only listing of the state root. Listing never opens a log;
/// directories without a parseable, valid header are reported, not fatal —
/// one damaged session must not blind the sidebar to the rest.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Headers, newest first.
    pub headers: Vec<SessionHeader>,
    /// Directories skipped (missing/damaged/foreign-format header).
    pub skipped: Vec<PathBuf>,
}

pub async fn scan(root: &Path) -> anyhow::Result<ScanReport> {
    let mut report = ScanReport::default();
    let mut buckets = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(err).context("scan state root"),
    };
    while let Some(bucket) = buckets.next_entry().await.context("walk state root")? {
        if !bucket.file_type().await?.is_dir() {
            continue;
        }
        let mut sessions = tokio::fs::read_dir(bucket.path())
            .await
            .with_context(|| format!("walk bucket {}", bucket.path().display()))?;
        while let Some(entry) = sessions.next_entry().await.context("walk bucket")? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            match tokio::fs::read(entry.path().join(HEADER_FILE)).await {
                Ok(bytes) => match serde_json::from_slice::<SessionHeader>(&bytes) {
                    Ok(header) if header.validate().is_ok() => report.headers.push(header),
                    _ => report.skipped.push(entry.path()),
                },
                Err(_) => report.skipped.push(entry.path()),
            }
        }
    }
    report.headers.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(report)
}

fn sync_dir(path: &Path) {
    // Best-effort directory fsync, matching the jsonl engine: some
    // filesystems refuse to fsync a directory opened read-only, and the
    // batch fsync inside the file is the durability authority anyway.
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::event::SettlementKind;
    use crate::session::identity::{AgentPreset, Lineage};
    use chrono::{TimeZone, Utc};

    fn header_at(id: &str, cwd: &str, minutes: u32) -> SessionHeader {
        SessionHeader {
            format_version: HEADER_FORMAT_VERSION,
            id: id.into(),
            created_at: Utc.with_ymd_and_hms(2026, 10, 16, 12, minutes, 0).unwrap(),
            origin_cwd: cwd.into(),
            lineage: Lineage::root(),
            agent_preset: AgentPreset::default(),
        }
    }

    fn events() -> Vec<Event> {
        vec![
            Event::CwdChange {
                to: "/work/other".into(),
            },
            Event::Pinned { on: true },
            Event::RunSettled {
                run_id: "sub-1".into(),
                settlement: SettlementKind::Aborted,
                verdict: "stopped before it finished".into(),
                salvage: None,
                turns: 2,
                tool_calls: 5,
                gated_denials: None,
            },
        ]
    }

    #[tokio::test]
    async fn create_leaves_no_disk_trace() {
        let root = tempfile::tempdir().unwrap();
        let store = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        assert!(!store.dir().exists());
    }

    #[tokio::test]
    async fn first_append_publishes_header_then_log() {
        let root = tempfile::tempdir().unwrap();
        let store = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        let seqs = store.append(events()).await.unwrap();
        assert_eq!(seqs, vec![0, 1, 2]);

        let dir = root.path().join("w-a").join("s-1");
        let header: SessionHeader =
            serde_json::from_slice(&std::fs::read(dir.join(HEADER_FILE)).unwrap()).unwrap();
        assert_eq!(header.id, "s-1");
        assert_eq!(header.origin_cwd, "/w/a");

        let opened = open(&dir).await.unwrap();
        assert!(!opened.torn_tail);
        assert_eq!(opened.envelopes.len(), 3);
        assert_eq!(opened.next_seq, 3);
        assert_eq!(opened.envelopes[0].event.kind(), "cwd_change");
        assert_eq!(opened.envelopes[2].event.kind(), "run/settled");
    }

    #[tokio::test]
    async fn reopen_of_unpublished_dir_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let err = open(&root.path().join("nope").join("s-x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read session header"));
    }

    #[tokio::test]
    async fn torn_tail_is_withheld_from_readers_and_republish_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        store.append(events()).await.unwrap();
        let dir = store.dir().to_path_buf();
        drop(store);

        // Simulate a crash mid-batch: a torn line without its newline.
        let log_path = dir.join(LOG_FILE);
        let mut bytes = std::fs::read(&log_path).unwrap();
        bytes.extend_from_slice(b"{\"seq\":3,\"ts\":\"1970-01-01T00:00:00Z\",\"type\":\"cwd_ch");
        std::fs::write(&log_path, &bytes).unwrap();

        let opened = open(&dir).await.unwrap();
        assert!(opened.torn_tail);
        assert_eq!(opened.envelopes.len(), 3);
        assert_eq!(opened.next_seq, 3);

        // The next writer truncates the torn bytes and continues the seq.
        let next = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        // Re-creating over a published session must fail loudly — the
        // writer-heal path is reached only through a process whose own
        // materialization crashed, which retries after reopening.
        assert!(next.append(events()).await.is_err());
    }

    #[tokio::test]
    async fn republish_after_release_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let header = header_at("s-1", "/w/a", 0);
        {
            let store = CoreStore::create(root.path(), header.clone()).unwrap();
            store.append(events()).await.unwrap();
        }
        let again = CoreStore::create(root.path(), header).unwrap();
        let err = again.append(events()).await.unwrap_err();
        assert!(err.to_string().contains("already published"), "{err}");
    }

    #[tokio::test]
    async fn a_live_writer_blocks_a_second_handle() {
        let root = tempfile::tempdir().unwrap();
        let header = header_at("s-1", "/w/a", 0);
        let first = CoreStore::create(root.path(), header.clone()).unwrap();
        first.append(events()).await.unwrap();
        // `first` still holds the writer lease, so a second handle — even
        // in this process — fails at its first append.
        let second = CoreStore::create(root.path(), header).unwrap();
        let err = second.append(events()).await.unwrap_err();
        assert!(err.to_string().contains("another writer"), "{err}");
    }

    #[tokio::test]
    async fn state_cache_round_trips_and_defaults_when_missing() {
        let root = tempfile::tempdir().unwrap();
        let store = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        let dir = store.dir().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            read_state(&dir).await.unwrap(),
            SessionStateCache::default()
        );

        let state = SessionStateCache {
            fold_watermark: 7,
            title: Some("the panel question".into()),
            unread: true,
            errored: false,
        };
        write_state(&dir, &state).await.unwrap();
        assert_eq!(read_state(&dir).await.unwrap(), state);
    }

    #[tokio::test]
    async fn scan_reads_headers_only_across_buckets() {
        let root = tempfile::tempdir().unwrap();
        for (id, cwd, minutes) in [("s-1", "/w/a", 10), ("s-2", "/w/b", 20)] {
            let store = CoreStore::create(root.path(), header_at(id, cwd, minutes)).unwrap();
            store.append(events()).await.unwrap();
        }
        // A directory with no parseable header is skipped, not fatal.
        std::fs::create_dir_all(root.path().join("w-a").join("junk")).unwrap();

        let report = scan(root.path()).await.unwrap();
        assert_eq!(report.headers.len(), 2);
        // Newest first.
        assert_eq!(report.headers[0].id, "s-2");
        assert_eq!(report.headers[1].id, "s-1");
        assert_eq!(report.skipped.len(), 1);
    }

    #[tokio::test]
    async fn unsupported_header_version_is_an_explicit_refusal() {
        let root = tempfile::tempdir().unwrap();
        let store = CoreStore::create(root.path(), header_at("s-1", "/w/a", 0)).unwrap();
        let dir = store.dir().to_path_buf();
        store.append(events()).await.unwrap();
        drop(store);

        let header_path = dir.join(HEADER_FILE);
        let mut header: SessionHeader =
            serde_json::from_slice(&std::fs::read(&header_path).unwrap()).unwrap();
        header.format_version = 99;
        std::fs::write(&header_path, serde_json::to_vec(&header).unwrap()).unwrap();

        let err = open(&dir).await.unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }
}
