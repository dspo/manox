// The journal's single writer.
//
// Every append is a message to one task that owns the write order: the
// caller sends a command and awaits a reply, and no caller holds a lock
// across file I/O. The writer resolves the parent, assigns the id, calls
// the storage's write face (which takes a `JournalWrite` it alone can
// mint), and replies. A dead writer drops every pending reply, so callers
// observe `JournalError::Closed` instead of a hang.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::{Event, SessionStorage};

/// Proof that the caller is the journal's single writer. The `()` field is
/// private, so only this module can mint one: the storage's write methods
/// cannot be called from an arbitrary task.
#[derive(Clone, Copy)]
pub struct JournalWrite(());

impl JournalWrite {
    /// Minted by the writer task alone — the single-writer invariant is the
    /// type's only constructor.
    fn mint() -> Self {
        JournalWrite(())
    }
}

/// Where a new entry attaches in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentIntent {
    /// Append onto the storage's current leaf.
    Tail,
    /// Append onto an explicit entry id (a branch point).
    Explicit(String),
    /// Append as a chain root.
    Root,
}

impl ParentIntent {
    /// The intent that keeps an entry's own parent — the verbatim append
    /// face (fork copies, fixture chains).
    pub(crate) fn of(entry: &Event) -> Self {
        match entry.parent_id() {
            Some(parent) => ParentIntent::Explicit(parent.to_string()),
            None => ParentIntent::Root,
        }
    }
}

/// One completed append: the entry's id and its chain depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    pub id: String,
    pub seq: u64,
}

/// Why a journal command could not complete.
#[derive(Debug)]
pub enum JournalError {
    /// The writer task is gone; the command can never be applied.
    Closed,
    /// The cross-process fence stayed held past its budget.
    Busy { path: PathBuf, budget: Duration },
    /// The file's header names a format this build does not read. The
    /// journal has one on-disk format; an older file is converted by the
    /// one-time operator migration, never by the runtime.
    UnsupportedFormat {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    /// The append failed for any other reason.
    Failed(anyhow::Error),
}

impl JournalError {
    /// Recover a typed journal error that travelled through `anyhow` (the
    /// storage write face is `anyhow`-shaped), so the typed variants stay
    /// matchable at the seam that produced them.
    pub(crate) fn recover(err: anyhow::Error) -> Self {
        match err.downcast::<JournalError>() {
            Ok(err) => err,
            Err(err) => JournalError::Failed(err),
        }
    }
}

impl From<anyhow::Error> for JournalError {
    fn from(err: anyhow::Error) -> Self {
        JournalError::recover(err)
    }
}

impl From<std::io::Error> for JournalError {
    fn from(err: std::io::Error) -> Self {
        JournalError::Failed(err.into())
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Closed => write!(f, "journal writer is closed"),
            JournalError::Busy { path, budget } => write!(
                f,
                "journal write fence stayed held for {budget:?}: {}",
                path.display()
            ),
            JournalError::UnsupportedFormat {
                path,
                found,
                supported,
            } => {
                if *found > *supported {
                    write!(
                        f,
                        "session journal {} is on format v{found}, written by a newer build; \
                         this binary reads up to v{supported}. Upgrade the binary to open it",
                        path.display()
                    )
                } else {
                    write!(
                        f,
                        "session journal {} is on format v{found}; this build reads only \
                         v{supported}. The file predates the current format and must be \
                         converted by the one-time operator migration \
                         (`script/migrate-journals.py`)",
                        path.display()
                    )
                }
            }
            JournalError::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Failed(err) => err.source(),
            _ => None,
        }
    }
}

/// The commands the writer task serializes, in channel order.
enum WriteCmd {
    Append {
        /// Boxed: the event is much larger than the other commands and the
        /// queue holds many of them.
        entry: Box<Event>,
        parent: ParentIntent,
        durable: bool,
        reply: oneshot::Sender<Result<Appended, JournalError>>,
    },
    AppendBatch {
        entries: Vec<Event>,
        reply: oneshot::Sender<Result<Vec<String>, JournalError>>,
    },
    SetLeaf {
        target: Option<String>,
        reply: oneshot::Sender<Result<(), JournalError>>,
    },
}

/// The append authority of one session: the storage it writes and the FIFO
/// queue into its writer task.
pub struct Journal<S> {
    storage: Arc<S>,
    /// The writer's command queue, started on the first write from inside
    /// the async append path. `Session::new` stays synchronous, so the task
    /// cannot be spawned at construction.
    tx: tokio::sync::OnceCell<mpsc::UnboundedSender<WriteCmd>>,
    /// The writer task's handle, kept so a test can kill it and observe the
    /// closed-writer contract. Production code never aborts the writer.
    #[cfg(test)]
    writer_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<S> Journal<S> {
    /// A journal over `storage`. The writer task starts with the first
    /// command, on that command's runtime.
    pub fn new(storage: Arc<S>) -> Arc<Self> {
        Arc::new(Journal {
            storage,
            tx: tokio::sync::OnceCell::new(),
            #[cfg(test)]
            writer_task: std::sync::Mutex::new(None),
        })
    }

    /// The storage this journal writes (reads share it).
    pub fn storage(&self) -> &Arc<S> {
        &self.storage
    }
}

impl<S: SessionStorage + 'static> Journal<S> {
    /// The writer's queue, spawning the task on first use. Without a tokio
    /// runtime there is no task to own the order, so the append is refused
    /// rather than run inline.
    async fn sender(&self) -> Result<&mpsc::UnboundedSender<WriteCmd>, JournalError> {
        self.tx
            .get_or_try_init(|| async {
                let runtime =
                    tokio::runtime::Handle::try_current().map_err(|_| JournalError::Closed)?;
                let (tx, rx) = mpsc::unbounded_channel();
                let storage = Arc::clone(&self.storage);
                let task = runtime.spawn(run_writer(storage, rx));
                #[cfg(test)]
                {
                    *self.writer_task.lock().unwrap() = Some(task);
                }
                #[cfg(not(test))]
                {
                    // A panicked writer kills the journal, and the JoinHandle
                    // is the only place tokio exposes that panic. Keep a
                    // detached monitor so the root cause is logged instead of
                    // every later append reporting only `Closed`.
                    runtime.spawn(async move {
                        if let Err(err) = task.await
                            && err.is_panic()
                        {
                            tracing::error!("journal writer panicked: {err}");
                        }
                    });
                }
                Ok(tx)
            })
            .await
    }

    /// Append one entry, resolving `parent` and assigning an id when the
    /// entry carries none. `durable` forces the storage to materialize the
    /// row before replying.
    pub(crate) async fn append(
        &self,
        entry: Event,
        parent: ParentIntent,
        durable: bool,
    ) -> Result<Appended, JournalError> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::Append {
            entry: Box::new(entry),
            parent,
            durable,
            reply,
        })
        .await?;
        rx.await.map_err(|_| JournalError::Closed)?
    }

    /// Append an entry exactly as built, keeping its id and parent. The
    /// storage's write face validates it like any other append.
    pub(crate) async fn append_verbatim(
        &self,
        entry: Event,
        durable: bool,
    ) -> Result<Appended, JournalError> {
        let parent = ParentIntent::of(&entry);
        self.append(entry, parent, durable).await
    }

    /// Append a prepared chain in one write. The entries carry their own ids
    /// and parents; the batch is one command so a deferred session
    /// materializes once.
    pub(crate) async fn append_batch(
        &self,
        entries: Vec<Event>,
    ) -> Result<Vec<String>, JournalError> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::AppendBatch { entries, reply }).await?;
        rx.await.map_err(|_| JournalError::Closed)?
    }

    /// Move the cursor by appending a `leaf` entry, through the same writer.
    pub(crate) async fn set_leaf(&self, target: Option<String>) -> Result<(), JournalError> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::SetLeaf { target, reply }).await?;
        rx.await.map_err(|_| JournalError::Closed)?
    }

    /// Kill the writer task — the death path tests. Every pending reply is
    /// dropped, so in-flight callers observe `Closed`.
    #[cfg(test)]
    pub(crate) fn abort_writer(&self) {
        if let Some(task) = self.writer_task.lock().unwrap().take() {
            task.abort();
        }
    }

    async fn send(&self, cmd: WriteCmd) -> Result<(), JournalError> {
        self.sender()
            .await?
            .send(cmd)
            .map_err(|_| JournalError::Closed)
    }
}

/// The writer loop: one command at a time, in arrival order. The reply
/// carries the outcome; a dropped receiver is the caller's own concern.
async fn run_writer<S: SessionStorage>(storage: Arc<S>, mut rx: mpsc::UnboundedReceiver<WriteCmd>) {
    let write = JournalWrite::mint();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            WriteCmd::Append {
                mut entry,
                parent,
                durable,
                reply,
            } => {
                let result = write_one(&*storage, write, &mut entry, parent, durable).await;
                let _ = reply.send(result);
            }
            WriteCmd::AppendBatch { entries, reply } => {
                let result = storage
                    .append_batch(&entries, write)
                    .await
                    .map_err(JournalError::recover)
                    .map(|()| entries.iter().map(|e| e.id().to_string()).collect());
                let _ = reply.send(result);
            }
            WriteCmd::SetLeaf { target, reply } => {
                let result = storage
                    .set_leaf_id(target.as_deref(), write)
                    .await
                    .map_err(JournalError::recover);
                let _ = reply.send(result);
            }
        }
    }
}

/// Resolve the parent, assign the id when the entry carries none, append,
/// and report `(id, seq)`.
async fn write_one<S: SessionStorage>(
    storage: &S,
    write: JournalWrite,
    entry: &mut Event,
    parent: ParentIntent,
    durable: bool,
) -> Result<Appended, JournalError> {
    let parent_id = match parent {
        ParentIntent::Tail => storage.get_leaf_id().await.map_err(JournalError::recover)?,
        ParentIntent::Explicit(id) => Some(id),
        ParentIntent::Root => None,
    };
    if entry.id().is_empty() {
        let id = storage
            .create_entry_id(write)
            .await
            .map_err(JournalError::recover)?;
        entry.set_id(id);
    }
    entry.set_parent_id(parent_id);
    let seq = if durable {
        storage.append_entry_durable(entry, write).await
    } else {
        storage.append_entry(entry, write).await
    }
    .map_err(JournalError::recover)?;
    Ok(Appended {
        id: entry.id().to_string(),
        seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
    use crate::session::{Event, Session};
    use crate::types::AgentMessage;
    use std::time::{Duration, Instant};

    fn metadata() -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        }
    }

    async fn storage_at(dir: &tempfile::TempDir) -> JsonlSessionStorage {
        JsonlSessionStorage::create(&dir.path().join("session.jsonl"), metadata())
            .await
            .unwrap()
    }

    /// Concurrent appenders serialize through one writer: the chain is dense,
    /// every entry parents onto the previous leaf, and no id repeats.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appenders_land_one_dense_chain() {
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(Session::new(storage_at(&dir).await));
        let mut tasks = Vec::new();
        for index in 0..16 {
            let session = Arc::clone(&session);
            tasks.push(tokio::spawn(async move {
                session
                    .append_message(AgentMessage::user(format!("m{index}")))
                    .await
                    .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.unwrap());
        }
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "no duplicate ids: {ids:?}");

        let entries = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert_eq!(entries.len(), ids.len(), "every append landed once");
        assert_eq!(entries[0].parent_id(), None, "the chain starts at a root");
        for pair in entries.windows(2) {
            assert_eq!(
                pair[1].parent_id(),
                Some(pair[0].id()),
                "each append parents onto the previous leaf"
            );
        }
    }

    /// An append issued from inside a journal subscriber's callback completes:
    /// the writer is a separate task, so no caller holds a lock the callback
    /// would need to re-enter.
    #[tokio::test]
    async fn append_from_a_subscriber_callback_completes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(storage_at(&dir).await);
        let mut events = storage.subscribe_journal();
        let session = Arc::new(Session::from_arc(Arc::clone(&storage)));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let callback_session = Arc::clone(&session);
        let callback = tokio::spawn(async move {
            let _event = events.recv().await.expect("one journal event");
            let appended = callback_session
                .append_message(AgentMessage::user("from the callback"))
                .await;
            let _ = done_tx.send(appended);
        });
        session
            .append_message(AgentMessage::user("first"))
            .await
            .unwrap();
        let appended = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("the callback's append must not deadlock")
            .expect("the callback reports its append");
        assert!(appended.is_ok(), "callback append failed: {appended:?}");
        callback.await.unwrap();
        assert_eq!(
            session
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// A dead writer resolves the in-flight append with `Closed` and refuses
    /// later appends immediately; reads keep working.
    #[tokio::test]
    async fn writer_death_resolves_pending_appends_with_closed() {
        use std::os::unix::io::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(storage_at(&dir).await);
        let journal = Journal::new(Arc::clone(&storage));
        let first = journal
            .append_verbatim(
                Event::Message {
                    id: "m1".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    message: AgentMessage::user("first"),
                    origin: None,
                    ui: None,
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(first.seq, 0);

        // Hold the write fence from another fd so the writer parks in the
        // fence retry loop; the append below is pending when the writer dies.
        let holder = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .open(storage.path())
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let pending = tokio::spawn({
            let journal = Arc::clone(&journal);
            async move {
                journal
                    .append_verbatim(
                        Event::Message {
                            id: "m2".into(),
                            parent_id: None,
                            timestamp: chrono::Utc::now(),
                            message: AgentMessage::user("pending"),
                            origin: None,
                            ui: None,
                        },
                        false,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        journal.abort_writer();
        let result = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("a dead writer must not hang the pending append")
            .unwrap();
        assert!(
            matches!(result, Err(JournalError::Closed)),
            "pending append must resolve as Closed, got {result:?}"
        );
        drop(holder);

        // Later appends fail immediately — no runtime wait, no panic.
        let started = Instant::now();
        let later = journal
            .append_verbatim(
                Event::Message {
                    id: "m3".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    message: AgentMessage::user("later"),
                    origin: None,
                    ui: None,
                },
                false,
            )
            .await;
        assert!(matches!(later, Err(JournalError::Closed)), "{later:?}");
        assert!(started.elapsed() < Duration::from_secs(1));

        // Reads still work off the storage.
        assert_eq!(
            storage.get_entries(Default::default()).await.unwrap().len(),
            1
        );
    }

    /// A fence held by another descriptor turns the append into `Busy` within
    /// the deadline instead of blocking the task forever.
    #[tokio::test]
    async fn a_held_write_fence_yields_busy_within_the_deadline() {
        use std::os::unix::io::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(storage_at(&dir).await);
        let journal = Journal::new(Arc::clone(&storage));
        let holder = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .open(storage.path())
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let started = Instant::now();
        let result = journal
            .append_verbatim(
                Event::Message {
                    id: "m1".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    message: AgentMessage::user("blocked"),
                    origin: None,
                    ui: None,
                },
                false,
            )
            .await;
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(JournalError::Busy { .. })),
            "a held fence must report Busy, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the append must give up on its budget, took {elapsed:?}"
        );
        drop(holder);
    }

    /// A writer that panics on its first append dies like any task: the
    /// pending append resolves `Closed` and later appends fail fast. (In
    /// production the detached monitor logs the panic; the contract here is
    /// the caller-visible one.)
    #[tokio::test]
    async fn a_panicking_writer_closes_pending_and_later_appends() {
        struct PanicStorage;

        #[async_trait::async_trait]
        impl SessionStorage for PanicStorage {
            async fn create_entry_id(&self, _: JournalWrite) -> Result<String, anyhow::Error> {
                Ok(uuid::Uuid::new_v4().to_string())
            }

            async fn append_entry(&self, _: &Event, _: JournalWrite) -> Result<u64, anyhow::Error> {
                panic!("writer exploded");
            }

            async fn get_entry(&self, _: &str) -> Result<Option<Event>, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
                Ok(None)
            }

            async fn set_leaf_id(
                &self,
                _: Option<&str>,
                _: JournalWrite,
            ) -> Result<(), anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_entries(
                &self,
                _: crate::session::SessionEntryCursor,
            ) -> Result<Vec<Event>, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn find_entries(
                &self,
                _: crate::session::EntryType,
            ) -> Result<Vec<Event>, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_label(&self, _: &str) -> Result<Option<String>, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_session_name(&self) -> Result<Option<String>, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_session_stats(
                &self,
            ) -> Result<crate::session::SessionStats, anyhow::Error> {
                unreachable!("not used by this test")
            }

            async fn get_path(&self, _: Option<&str>) -> Result<Vec<Event>, anyhow::Error> {
                unreachable!("not used by this test")
            }
        }

        let session = Session::new(PanicStorage);
        let err = session
            .append_label("e", Some("boom".into()))
            .await
            .expect_err("the panicking writer cannot answer");
        assert!(
            matches!(
                err.downcast_ref::<JournalError>(),
                Some(JournalError::Closed)
            ),
            "the pending append resolves Closed: {err:#}"
        );
        let err = session
            .append_label("e2", Some("boom".into()))
            .await
            .expect_err("a dead writer fails fast");
        assert!(
            matches!(
                err.downcast_ref::<JournalError>(),
                Some(JournalError::Closed)
            ),
            "later appends resolve Closed: {err:#}"
        );
    }
}
