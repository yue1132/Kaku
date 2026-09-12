//! Background file transfers between the local filesystem and a remote
//! sftp session.
//!
//! Transfers run on dedicated threads, stream data in chunks, report
//! progress through a channel, and can be cancelled mid-flight.  The
//! [TransferManager] is cheap to clone and is intended to be shared
//! between the sftp overlay UI and background entry points such as
//! drag-and-drop upload.

use smol::channel::{bounded, Receiver, Sender, TrySendError};
use smol::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use wezterm_ssh::{Metadata, Session, Sftp, Utf8PathBuf};

/// How many transfers may copy data at the same time, one per lane.
/// Additional transfers queue until a slot frees up so that a large
/// queue cannot starve the connection.
const MAX_CONCURRENT_TRANSFERS: usize = 4;

/// Copy chunk size: the largest single SFTP write/read packet OpenSSH
/// advertises (`SFTP_MAX_MSG_LENGTH - 1024` = 256KiB - 1KiB), so one
/// chunk is exactly one packet.  Every packet costs one round trip
/// through the ssh session thread, so a buffer above this limit would
/// pay a second (mostly empty) round trip, and a buffer below it would
/// pay more round trips for the same bytes.  Servers that advertise
/// nothing cap out at 32KiB and simply accept less per call.
const CHUNK_SIZE: usize = 261120;

/// Prefix of the in-progress file.  Transfers write here and publish onto
/// the real destination only once every byte is in place, so an
/// interrupted or cancelled transfer never leaves a truncated destination
/// behind, and a later resume continues from the partial file.  The
/// leading dot keeps part files out of directory listings, which hide
/// dotfiles by default.
const PART_PREFIX: &str = ".kaku-part.";

/// Remote part file for `dest`, next to it so a rename is a real rename.
pub(crate) fn remote_part(dest: &Utf8PathBuf) -> Utf8PathBuf {
    match dest.file_name() {
        Some(name) => match dest.parent() {
            Some(dir) => dir.join(format!("{PART_PREFIX}{name}")),
            None => Utf8PathBuf::from(format!("{PART_PREFIX}{name}")),
        },
        None => Utf8PathBuf::from(format!("{PART_PREFIX}{dest}")),
    }
}

/// Local part file for `dest`.
pub(crate) fn local_part(dest: &Path) -> PathBuf {
    let mut part = dest.to_path_buf();
    if let Some(name) = dest.file_name() {
        part.set_file_name(format!("{PART_PREFIX}{}", name.to_string_lossy()));
    }
    part
}

/// Byte range for worker `index` of `workers`.  The division remainder
/// lands in the last slice, so the ranges tile `[0, total)` exactly.
fn slice_range(total: u64, workers: usize, index: usize) -> (u64, u64) {
    let slice = total / workers as u64;
    let start = slice * index as u64;
    let len = if index + 1 == workers {
        total - start
    } else {
        slice
    };
    (start, len)
}

/// Move a finished part file onto `dest`.  OpenSSH replaces an existing
/// destination; servers that follow the draft strictly refuse, so drop the
/// old file and retry.  That window is far smaller than the
/// truncated-destination window this scheme removes.
pub(crate) async fn publish_remote(
    sftp: &Sftp,
    part: &Utf8PathBuf,
    dest: &Utf8PathBuf,
) -> Result<(), TransferError> {
    let rename = |from: &str, to: &str| {
        let (from, to) = (from.to_string(), to.to_string());
        let sftp = sftp.clone();
        async move {
            sftp.rename(
                from.as_str(),
                to.as_str(),
                wezterm_ssh::RenameOptions::default(),
            )
            .await
        }
    };

    if rename(part.as_str(), dest.as_str()).await.is_ok() {
        return Ok(());
    }

    // Some servers refuse to overwrite an existing target.  Move the old
    // file aside instead of deleting it: deleting first meant that a second
    // failure (a transient error, not "already exists") left the user with
    // neither the new file nor the old one.
    let backup = format!("{dest}.kaku-old");
    let had_dest = rename(dest.as_str(), &backup).await.is_ok();
    match rename(part.as_str(), dest.as_str()).await {
        Ok(()) => {
            if had_dest {
                sftp.remove_file(backup.as_str()).await.ok();
            }
            Ok(())
        }
        Err(err) => {
            if had_dest {
                // Put the original back where it was.
                rename(&backup, dest.as_str()).await.ok();
            }
            Err(TransferError::from(err))
        }
    }
}

/// Move a finished local part file onto `dest` (atomic on one volume).
fn publish_local(part: &Path, dest: &Path) -> Result<(), TransferError> {
    std::fs::rename(part, dest).map_err(TransferError::from)
}

/// A single read or write may not park a worker forever.  A half-dead
/// connection leaves the future pending, cancellation is only checked
/// between chunks, and the lane, the thread and the progress row would
/// then stay stuck until the app was restarted.
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const IO_STALLED: &str = "connection stalled (no data for 60s)";

/// Run one I/O future under [IO_TIMEOUT], mapping the timeout to `E`.
async fn with_io_timeout<T, E, F>(fut: F, timed_out: impl FnOnce() -> E) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    smol::future::or(fut, async {
        smol::Timer::after(IO_TIMEOUT).await;
        Err(timed_out())
    })
    .await
}

/// Progress events are throttled so a fast link cannot flood the event
/// channel; the final byte count is always reported.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Event channel capacity.  Progress updates are dropped (they never
/// block a transfer) when the consumer is slower than this; terminal
/// events always block until delivered.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Once finished, statuses are pruned when the list grows past this.
const MAX_RETAINED_STATUSES: usize = 64;

pub type TransferId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
    /// Remote to remote, e.g. pasting a copied file in the browser.
    Copy,
}

impl Direction {
    /// Glyph drawn next to a transfer in the UI.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Upload => "↑",
            Self::Download => "↓",
            Self::Copy => "⇄",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running { bytes: u64, total: u64 },
    Done,
    Failed(String),
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_) | Self::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct TransferStatus {
    pub id: TransferId,
    pub direction: Direction,
    pub source: String,
    pub dest: String,
    pub state: TransferState,
}

/// Events emitted by [TransferManager] as transfers progress.  Progress
/// details live in [TransferManager::statuses]; `Updated` is only a
/// wake-up signal and `Finished` carries the terminal status.
#[derive(Clone, Debug)]
pub enum TransferEvent {
    Updated,
    Finished(TransferStatus),
}

#[derive(Debug)]
pub(crate) enum TransferError {
    Cancelled,
    /// The destination exists and this transfer was not allowed to
    /// replace it (a Finder drop onto an existing file).
    AlreadyExists(String),
    Failed(anyhow::Error),
}

/// Why a parallel transfer gave up, kept as an enum so a cancelled
/// transfer is not reported as a failure.
#[derive(Debug)]
enum SliceFailure {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::AlreadyExists(path) => {
                write!(
                    f,
                    "{path} already exists; drop it somewhere else or rename it"
                )
            }
            Self::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<anyhow::Error> for TransferError {
    fn from(err: anyhow::Error) -> Self {
        Self::Failed(err)
    }
}

impl From<std::io::Error> for TransferError {
    fn from(err: std::io::Error) -> Self {
        Self::Failed(err.into())
    }
}

impl From<wezterm_ssh::SftpChannelError> for TransferError {
    fn from(err: wezterm_ssh::SftpChannelError) -> Self {
        Self::Failed(err.into())
    }
}

enum WorkerJob {
    Upload {
        local: PathBuf,
        remote: Utf8PathBuf,
        resume: bool,
        /// When false the upload refuses to replace an existing remote
        /// file.  The Finder drop paths use this so dragging a file over
        /// one that is already there can never clobber it silently.
        overwrite: bool,
    },
    /// Copy one remote file to another remote path.
    RemoteCopy { from: Utf8PathBuf, to: Utf8PathBuf },
    Download {
        remote: Utf8PathBuf,
        local: PathBuf,
        resume: bool,
    },
}

struct TransferInner {
    sftp: Sftp,
    /// The main session; kept for future helper re-auth/reconnect
    /// work.  Parallel slices use their own helper sessions.
    #[allow(dead_code)]
    main_session: Session,
    next_id: AtomicU64,
    statuses: Mutex<Vec<TransferStatus>>,
    cancels: Mutex<HashMap<TransferId, Arc<AtomicBool>>>,
    /// Remote directories already ensured, so recursive uploads do not
    /// pay a mkdir round-trip per file.
    created_dirs: Mutex<HashSet<String>>,
    helper_target: Mutex<Option<String>>,
    helpers: Mutex<Vec<Session>>,
    /// Set once auxiliary connections have failed for this target, so a
    /// password-only host does not pay a failed handshake per big file.
    helpers_unavailable: AtomicBool,
    /// Which lanes currently have a transfer running on them.  A lane is
    /// one ssh connection, so two transfers sharing one lane would each
    /// wait for the other's round trips.
    lane_busy: Mutex<Vec<bool>>,
}

/// Marks a lane busy while a transfer runs on it.
struct LaneGuard {
    inner: Arc<TransferInner>,
    index: usize,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.inner.lane_busy.lock().unwrap().get_mut(self.index) {
            *slot = false;
        }
    }
}

/// Files at or above this size transfer through multiple parallel
/// connections.
const PARALLEL_THRESHOLD: u64 = 16 * 1024 * 1024;
/// Total workers for a parallel transfer, including the main session.
const PARALLEL_WORKERS: usize = 4;

impl TransferInner {
    /// Drop the helper pool so the next parallel transfer dials fresh
    /// connections.  A helper that died mid-slice would otherwise stay in
    /// the pool and fail every later transfer until the panel restarts.
    fn invalidate_helpers(&self) {
        self.helpers.lock().unwrap().clear();
        self.helpers_unavailable.store(false, Ordering::Relaxed);
    }

    /// Establish (once) and return the auxiliary helper sessions used
    /// for parallel range transfers.  Fewer sessions are returned when
    /// extra connections cannot authenticate non-interactively.
    fn ensure_helpers(&self) -> Vec<Session> {
        let Some(target) = self.helper_target.lock().unwrap().clone() else {
            return Vec::new();
        };
        let mut sessions = self.helpers.lock().unwrap();
        if !sessions.is_empty() {
            return sessions.clone();
        }
        if self.helpers_unavailable.load(Ordering::Relaxed) {
            return Vec::new();
        }
        while sessions.len() < PARALLEL_WORKERS - 1 {
            match crate::sftp_sessions::connect_noninteractive(&target) {
                Ok(s) => sessions.push(s),
                Err(err) => {
                    log::info!("sftp parallel: helper connect failed: {err}");
                    break;
                }
            }
        }
        if sessions.is_empty() {
            // Key/agent auth is not available here; stop asking on every
            // subsequent transfer.
            self.helpers_unavailable.store(true, Ordering::Relaxed);
        }
        sessions.clone()
    }

    /// Update a transfer's state.  `None` means the entry is gone, which
    /// can only happen for a late progress tick from a transfer that
    /// already finished and was pruned; it must not panic a background
    /// thread.  In-flight transfers are never pruned, so the terminal
    /// update always finds its entry.
    /// Number of transfers that have not finished yet, including the
    /// one asking.
    fn in_flight(&self) -> usize {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|status| !status.state.is_terminal())
            .count()
    }

    /// Connection this transfer should run on, held for its duration.
    /// Every lane is its own ssh connection, so their round trips overlap
    /// instead of queueing behind one session thread; that is what makes
    /// a folder of small files several times faster.
    ///
    /// Extra connections are opened only when more than one transfer is
    /// queued: a single dragged file must not pay for handshakes it
    /// cannot use.
    fn acquire_lane(self: &Arc<Self>, id: TransferId) -> (Sftp, LaneGuard) {
        let helpers = if self.in_flight() > 1 {
            self.ensure_helpers()
        } else {
            Vec::new()
        };
        let lanes = helpers.len() + 1;
        let index = {
            let mut busy = self.lane_busy.lock().unwrap();
            let free = (0..lanes.min(busy.len())).find(|index| !busy[*index]);
            // Falling back to a shared lane is only possible when the
            // auxiliary connections are unavailable.
            let index = free.unwrap_or(id as usize % lanes);
            if let Some(slot) = busy.get_mut(index) {
                *slot = true;
            }
            index
        };
        let sftp = match helpers.get(index.wrapping_sub(1)) {
            Some(session) => session.sftp(),
            None => self.sftp.clone(),
        };
        (
            sftp,
            LaneGuard {
                inner: Arc::clone(self),
                index,
            },
        )
    }

    /// Refuse a transfer without running it: used when the same
    /// destination is already in flight, where two writers would fight
    /// over the same part file.
    fn reject(&self, id: TransferId, reason: &str, event_tx: &Sender<TransferEvent>) {
        if let Some(status) = self.update_status(id, TransferState::Failed(reason.to_string())) {
            self.finish(id);
            let _ = event_tx.try_send(TransferEvent::Finished(status));
        }
    }

    fn update_status(&self, id: TransferId, state: TransferState) -> Option<TransferStatus> {
        let mut statuses = self.statuses.lock().unwrap();
        let status = statuses.iter_mut().find(|s| s.id == id)?;
        status.state = state;
        Some(status.clone())
    }

    fn emit_progress(
        &self,
        event_tx: &Sender<TransferEvent>,
        id: TransferId,
        bytes: u64,
        total: u64,
    ) {
        {
            let mut statuses = self.statuses.lock().unwrap();
            match statuses.iter_mut().find(|s| s.id == id) {
                // A ticker that was mid-sleep when the transfer finished
                // must not downgrade the terminal state back to running.
                Some(status) if !status.state.is_terminal() => {
                    status.state = TransferState::Running { bytes, total };
                }
                _ => return,
            }
        }
        if let Err(TrySendError::Full(_)) = event_tx.try_send(TransferEvent::Updated) {
            // The consumer is slow; dropping an intermediate progress
            // tick is fine, the next one carries fresher data.
        }
    }

    fn finish(&self, id: TransferId) {
        self.cancels.lock().unwrap().remove(&id);
        let mut statuses = self.statuses.lock().unwrap();
        if statuses.len() > MAX_RETAINED_STATUSES {
            statuses.retain(|s| !s.state.is_terminal());
            statuses.shrink_to(16);
        }
    }
}

/// Shared handle to the set of in-flight and recently finished transfers.
///
/// The manager owns the remote side's [Sftp] handle; operations are
/// fire-and-forget from the caller's perspective, with results arriving
/// through [TransferManager::events].
#[derive(Clone)]
pub struct TransferManager {
    inner: Arc<TransferInner>,
    permits_tx: Sender<()>,
    permits_rx: Receiver<()>,
    event_tx: Sender<TransferEvent>,
    event_rx: Receiver<TransferEvent>,
}

impl TransferManager {
    pub fn new(session: Session) -> Self {
        // Semaphore built from a channel pre-filled with tokens: workers
        // recv() to acquire and try_send() a token back to release.
        let (permits_tx, permits_rx) = bounded(MAX_CONCURRENT_TRANSFERS);
        for _ in 0..MAX_CONCURRENT_TRANSFERS {
            permits_tx.try_send(()).ok();
        }
        let (event_tx, event_rx) = bounded(EVENT_CHANNEL_CAPACITY);
        let sftp = session.sftp();
        let inner = Arc::new(TransferInner {
            sftp,
            main_session: session,
            next_id: AtomicU64::new(1),
            statuses: Mutex::new(Vec::new()),
            cancels: Mutex::new(HashMap::new()),
            created_dirs: Mutex::new(HashSet::new()),
            helper_target: Mutex::new(None),
            helpers: Mutex::new(Vec::new()),
            helpers_unavailable: AtomicBool::new(false),
            lane_busy: Mutex::new(vec![false; MAX_CONCURRENT_TRANSFERS]),
        });
        Self {
            inner,
            permits_tx,
            permits_rx,
            event_tx,
            event_rx,
        }
    }

    /// Set the connect target used to establish auxiliary helper
    /// connections for parallel range transfers.
    pub fn set_helper_target(&self, target: String) {
        *self.inner.helper_target.lock().unwrap() = Some(target);
    }

    /// Queue a file upload from `local` to `remote`.
    pub fn upload(
        &self,
        local: impl Into<PathBuf>,
        remote: impl Into<Utf8PathBuf>,
        resume: bool,
        overwrite: bool,
    ) -> TransferId {
        let (local, remote) = (local.into(), remote.into());
        let id = self.register(
            Direction::Upload,
            &local.display().to_string(),
            remote.as_str(),
        );
        if self.destination_busy(id, remote.as_str()) {
            self.inner
                .reject(id, "this file is already being transferred", &self.event_tx);
            return id;
        }
        self.spawn(
            id,
            WorkerJob::Upload {
                local,
                remote,
                resume,
                overwrite,
            },
        );
        id
    }

    /// Queue a file download from `remote` to `local`.
    pub fn download(
        &self,
        remote: impl Into<Utf8PathBuf>,
        local: impl Into<PathBuf>,
        resume: bool,
    ) -> TransferId {
        let (remote, local) = (remote.into(), local.into());
        let id = self.register(
            Direction::Download,
            remote.as_str(),
            &local.display().to_string(),
        );
        if self.destination_busy(id, &local.display().to_string()) {
            self.inner
                .reject(id, "this file is already being transferred", &self.event_tx);
            return id;
        }
        self.spawn(
            id,
            WorkerJob::Download {
                remote,
                local,
                resume,
            },
        );
        id
    }

    /// Copy `from` to `to`, both on the remote side.  SFTP has no
    /// server-side copy, so the bytes stream through this connection.
    pub fn copy_remote(
        &self,
        from: impl Into<Utf8PathBuf>,
        to: impl Into<Utf8PathBuf>,
    ) -> TransferId {
        let (from, to) = (from.into(), to.into());
        let id = self.register(Direction::Copy, from.as_str(), to.as_str());
        if self.destination_busy(id, to.as_str()) {
            self.inner
                .reject(id, "this file is already being transferred", &self.event_tx);
            return id;
        }
        self.spawn(id, WorkerJob::RemoteCopy { from, to });
        id
    }

    /// Drop finished transfers from the list (the `w` task manager uses
    /// this to clear the history without touching anything still running).
    pub fn clear_finished(&self) {
        let mut statuses = self.inner.statuses.lock().unwrap();
        statuses.retain(|status| !status.state.is_terminal());
    }

    /// Ask a transfer to stop at the next chunk boundary.  Partial
    /// output files are removed.
    pub fn cancel(&self, id: TransferId) {
        if let Some(flag) = self.inner.cancels.lock().unwrap().get(&id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Current snapshot of queued, running, and recently finished transfers.
    pub fn statuses(&self) -> Vec<TransferStatus> {
        self.inner.statuses.lock().unwrap().clone()
    }

    /// The event stream drained by the UI; see [TransferEvent].
    pub fn events(&self) -> &Receiver<TransferEvent> {
        &self.event_rx
    }

    /// True when another unfinished transfer already writes `dest`.
    fn destination_busy(&self, id: TransferId, dest: &str) -> bool {
        self.inner
            .statuses
            .lock()
            .unwrap()
            .iter()
            .any(|status| status.id != id && !status.state.is_terminal() && status.dest == dest)
    }

    fn register(&self, direction: Direction, source: &str, dest: &str) -> TransferId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.statuses.lock().unwrap().push(TransferStatus {
            id,
            direction,
            source: source.to_string(),
            dest: dest.to_string(),
            state: TransferState::Queued,
        });
        self.inner
            .cancels
            .lock()
            .unwrap()
            .insert(id, Arc::new(AtomicBool::new(false)));
        id
    }

    fn spawn(&self, id: TransferId, job: WorkerJob) {
        let inner = self.inner.clone();
        let inner_for_spawn_err = self.inner.clone();
        let permits_tx = self.permits_tx.clone();
        let permits_rx = self.permits_rx.clone();
        let event_tx = self.event_tx.clone();
        let event_tx_for_spawn_err = self.event_tx.clone();
        let cancel = inner
            .cancels
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .expect("transfer registered before spawn");
        let name = match &job {
            WorkerJob::Upload { .. } => format!("sftp-upload-{id}"),
            WorkerJob::Download { .. } => format!("sftp-download-{id}"),
            WorkerJob::RemoteCopy { .. } => format!("sftp-copy-{id}"),
        };
        let spawned = thread::Builder::new().name(name).spawn(move || {
            smol::block_on(async move {
                // Acquire a concurrency slot; the manager being
                // dropped means nobody is left to transfer for.
                let _permit = permits_rx.recv().await;
                // The lane is released as soon as the copy finishes: the
                // terminal event send below can block on a full channel,
                // and holding a connection through that would stall the
                // transfers queued behind it.
                let result = {
                    let (lane, _lane_guard) = inner.acquire_lane(id);
                    run_job(
                        inner.clone(),
                        lane,
                        job,
                        id,
                        cancel.clone(),
                        event_tx.clone(),
                    )
                    .await
                };
                // Release the slot back for queued transfers.
                permits_tx.try_send(()).ok();
                let state = match result {
                    Ok(()) => TransferState::Done,
                    Err(TransferError::Cancelled) => TransferState::Cancelled,
                    Err(err @ TransferError::AlreadyExists(_)) => {
                        TransferState::Failed(err.to_string())
                    }
                    Err(TransferError::Failed(err)) => TransferState::Failed(format!("{err:#}")),
                };
                if let Some(status) = inner.update_status(id, state) {
                    inner.finish(id);
                    // Terminal events must be delivered even under
                    // back-pressure, so this send may block.
                    event_tx.send(TransferEvent::Finished(status)).await.ok();
                } else {
                    inner.finish(id);
                }
            });
        });
        if let Err(err) = spawned {
            let terminal = inner_for_spawn_err
                .update_status(
                    id,
                    TransferState::Failed(format!("failed to spawn worker thread: {err}")),
                )
                .map(TransferEvent::Finished);
            inner_for_spawn_err.finish(id);
            if let Some(event) = terminal {
                let _ = event_tx_for_spawn_err.try_send(event);
            }
        }
    }
}

async fn run_job(
    inner: Arc<TransferInner>,
    lane: Sftp,
    job: WorkerJob,
    id: TransferId,
    cancel: Arc<AtomicBool>,
    event_tx: Sender<TransferEvent>,
) -> Result<(), TransferError> {
    let result = match job {
        WorkerJob::Upload {
            local,
            remote,
            resume,
            overwrite,
        } => {
            let st = std::fs::metadata(&local)?;
            let total = st.len();
            let part = remote_part(&remote);
            // Refuse to replace a file that is already there unless the
            // caller asked for it (the F5 and paste paths asked the user
            // first; the Finder drop paths deliberately did not).
            if !overwrite && lane.metadata(remote.as_str()).await.is_ok() {
                return Err(TransferError::AlreadyExists(remote.to_string()));
            }
            if total >= PARALLEL_THRESHOLD && !resume {
                return run_upload_parallel(
                    inner, lane, local, remote, total, id, event_tx, cancel,
                );
            }
            let start = if resume {
                // Resume continues the part file, never the destination:
                // the destination only ever appears complete.
                let existing = lane
                    .metadata(part.as_str())
                    .await
                    .ok()
                    .and_then(|m| m.size)
                    .filter(|size| *size < total)
                    .unwrap_or(0);
                inner.emit_progress(&event_tx, id, existing, total);
                existing
            } else {
                inner.emit_progress(&event_tx, id, 0, total);
                0
            };
            run_upload(
                &lane,
                &inner.created_dirs,
                &local,
                &remote,
                start,
                &cancel,
                &mut |bytes| inner.emit_progress(&event_tx, id, bytes, total),
            )
            .await
        }
        WorkerJob::RemoteCopy { from, to } => {
            let meta = lane.metadata(from.as_str()).await?;
            let total = meta.size.unwrap_or(0);
            inner.emit_progress(&event_tx, id, 0, total);
            run_remote_copy(
                &lane,
                &inner.created_dirs,
                &from,
                &to,
                &meta,
                &cancel,
                &mut |bytes| inner.emit_progress(&event_tx, id, bytes, total),
            )
            .await
        }
        WorkerJob::Download {
            remote,
            local,
            resume,
        } => {
            let meta = lane.metadata(remote.as_str()).await?;
            let total = meta.size.unwrap_or(0);
            if total >= PARALLEL_THRESHOLD && !resume {
                return run_download_parallel(
                    inner, lane, remote, local, total, id, event_tx, cancel,
                );
            }
            let part = local_part(&local);
            let start = if resume {
                let existing = std::fs::metadata(&part)
                    .map(|m| m.len())
                    .ok()
                    .filter(|size| *size < total)
                    .unwrap_or(0);
                inner.emit_progress(&event_tx, id, existing, total);
                existing
            } else {
                inner.emit_progress(&event_tx, id, 0, total);
                0
            };
            run_download(
                &lane,
                &remote,
                &meta,
                &local,
                start,
                &cancel,
                &mut |bytes| inner.emit_progress(&event_tx, id, bytes, total),
            )
            .await
        }
    };
    // Cancelled and failed partial files are kept on purpose: the
    // resume path continues from the remote/local size instead of
    // restarting large transfers from zero.
    result
}

async fn run_upload(
    sftp: &Sftp,
    created_dirs: &Mutex<HashSet<String>>,
    local: &Path,
    remote: &Utf8PathBuf,
    start: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), TransferError> {
    let part = remote_part(remote);
    let part = &part;
    let st = std::fs::metadata(local)?;
    // Carry the source mode on the OPEN request itself: a separate
    // setstat is one more round trip per file, which dominates when a
    // folder of small files is uploaded over a link with any latency.
    // The remote mtime is deliberately not preserved: it would need that
    // extra round trip, and a fresh mtime is what build tools want after
    // an upload.
    let mode = (st.permissions().mode() & 0o777).max(0o600) as i32;
    ensure_remote_parent(created_dirs, sftp, remote).await;
    let mut reader = smol::fs::File::open(local).await?;
    let mut writer = if start > 0 {
        // Resume: append from the part file's current end.
        let writer = sftp
            .open_with_mode(
                part,
                wezterm_ssh::OpenOptions {
                    read: false,
                    write: Some(wezterm_ssh::WriteMode::Append),
                    mode: 0o644,
                    ty: wezterm_ssh::OpenFileType::File,
                },
            )
            .await?;
        reader
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!("seek: {e}")))?;
        writer
    } else {
        sftp.open_with_mode(
            part,
            wezterm_ssh::OpenOptions {
                read: false,
                write: Some(wezterm_ssh::WriteMode::Write),
                mode,
                ty: wezterm_ssh::OpenFileType::File,
            },
        )
        .await?
    };
    copy_stream(&mut reader, &mut writer, start, cancel, on_progress).await?;
    // No flush: on this path flush is fsync@openssh.com, a whole extra
    // round trip plus a server-side disk sync per file.  Every write is
    // already acknowledged by the server before the next one is sent.
    writer.close().await?;
    publish_remote(sftp, part, remote).await
}

async fn run_download(
    sftp: &Sftp,
    remote: &Utf8PathBuf,
    meta: &Metadata,
    local: &Path,
    start: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), TransferError> {
    let part = local_part(local);
    let part = part.as_path();
    ensure_local_parent(part).await?;
    let mut reader = sftp.open(remote).await?;
    if start > 0 {
        reader
            .seek(start)
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!("seek: {e}")))?;
    }
    let mut writer = if start > 0 {
        smol::fs::OpenOptions::new().append(true).open(part).await?
    } else {
        smol::fs::File::create(part).await?
    };
    copy_stream(&mut reader, &mut writer, start, cancel, on_progress).await?;
    writer.flush().await?;
    writer.close().await?;

    // Preserve the remote execute bits on the local copy so downloaded
    // scripts and binaries remain runnable.  Read/write bits come from
    // the remote mode too; the umask does not apply after the fact.
    if let Some(perms) = meta.permissions {
        let mode = perms.to_unix_mode() & 0o777;
        if mode != 0 {
            std::fs::set_permissions(part, std::fs::Permissions::from_mode(mode)).ok();
        }
    }
    publish_local(part, local)
}

/// Copy one remote file onto another remote path.  The reader and the
/// writer share a connection, so each chunk costs a read round trip plus
/// a write round trip; that is still far cheaper than routing the bytes
/// through the local disk.  The destination is published atomically.
async fn run_remote_copy(
    sftp: &Sftp,
    created_dirs: &Mutex<HashSet<String>>,
    from: &Utf8PathBuf,
    to: &Utf8PathBuf,
    meta: &Metadata,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), TransferError> {
    let part = remote_part(to);
    ensure_remote_parent(created_dirs, sftp, to).await;
    let mode = meta
        .permissions
        .map(|perms| (perms.to_unix_mode() & 0o777).max(0o600) as i32)
        .unwrap_or(0o600);
    let mut reader = sftp.open(from).await?;
    let mut writer = sftp
        .open_with_mode(
            &part,
            wezterm_ssh::OpenOptions {
                read: false,
                write: Some(wezterm_ssh::WriteMode::Write),
                mode,
                ty: wezterm_ssh::OpenFileType::File,
            },
        )
        .await?;
    copy_stream(&mut reader, &mut writer, 0, cancel, on_progress).await?;
    writer.close().await?;
    publish_remote(sftp, &part, to).await
}

/// Create the local directory a download lands in.  A remote tree can
/// be deeper than anything that exists locally.
async fn ensure_local_parent(part: &Path) -> Result<(), TransferError> {
    match part.parent() {
        Some(dir) => Ok(smol::fs::create_dir_all(dir).await?),
        None => Ok(()),
    }
}

/// Create every missing parent directory of `remote` (mkdir -p).
/// Results are cached per manager so recursive uploads do not pay a
/// round-trip per file.
async fn ensure_remote_parent(
    created_dirs: &Mutex<HashSet<String>>,
    sftp: &Sftp,
    remote: &Utf8PathBuf,
) {
    let mut prefix = String::new();
    for component in remote.iter() {
        let comp: &str = component;
        if comp == "/" {
            prefix.push('/');
            continue;
        }
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        prefix.push_str(comp);
        if created_dirs.lock().unwrap().contains(&prefix) {
            continue;
        }
        // The final component is the file itself, created by the
        // writer; only ancestors get directories.
        if prefix.as_str() != remote.as_str() {
            sftp.create_dir(&prefix, 0o755).await.ok();
        }
        created_dirs.lock().unwrap().insert(prefix.clone());
    }
}

/// Split-range parallel upload: pre-allocate the remote file, then
/// every worker (main session + helpers) writes its own byte range
/// through its own connection.
// One worker per slice plus the job's identity and its channel:
// a struct would only move these fields around.
#[allow(clippy::too_many_arguments)]
fn run_upload_parallel(
    inner: Arc<TransferInner>,
    lane: Sftp,
    local: PathBuf,
    remote: Utf8PathBuf,
    total: u64,
    id: TransferId,
    event_tx: Sender<TransferEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<(), TransferError> {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), SliceFailure>>();
    let spawned = std::thread::Builder::new()
        .name("sftp-parallel-upload".into())
        .spawn(move || {
            let part = remote_part(&remote);
            let res: Result<(), TransferError> = smol::block_on(async {
                // Same mkdir -p as the single-stream path: a folder upload
                // can reach a 16MB+ file before any sibling created its
                // directory.
                ensure_remote_parent(&inner.created_dirs, &lane, &remote).await;
                // Truncate the part file so the slices can seek-write into
                // it.  The length is not preallocated: every slice writes
                // its range, so the file reaches `total` on its own.  The
                // mode rides the OPEN so no setstat round trip is needed.
                let mode = std::fs::metadata(&local)
                    .map(|m| (m.permissions().mode() & 0o777).max(0o600) as i32)
                    .unwrap_or(0o600);
                let mut w = inner
                    .sftp
                    .open_with_mode(
                        &part,
                        wezterm_ssh::OpenOptions {
                            read: false,
                            write: Some(wezterm_ssh::WriteMode::Write),
                            mode,
                            ty: wezterm_ssh::OpenFileType::File,
                        },
                    )
                    .await?;
                w.flush().await?;
                w.close().await?;
                Ok(())
            });
            if let Err(err) = res {
                let _ = done_tx.send(Err(SliceFailure::Failed(format!("{err:#}"))));
                return;
            }

            // Report the file as started before connecting helpers, so a
            // slow auxiliary handshake does not look like a frozen queue.
            inner.emit_progress(&event_tx, id, 0, total);
            let helpers = inner.ensure_helpers();
            let workers = helpers.len() + 1;
            let uploaded = Arc::new(AtomicU64::new(0));
            let done = Arc::new(AtomicU64::new(0));
            let mut first_err: Option<String> = None;

            let ticker_done = Arc::new(AtomicBool::new(false));
            {
                let inner = inner.clone();
                let uploaded = uploaded.clone();
                let done = done.clone();
                let ticker_done = ticker_done.clone();
                let event_tx = event_tx.clone();
                std::thread::Builder::new()
                    .name("sftp-parallel-progress".into())
                    .spawn(move || {
                        // `ticker_done` is the exit that always happens:
                        // a panicking slice thread never bumps `done`.
                        while !ticker_done.load(Ordering::Relaxed)
                            && done.load(Ordering::Relaxed) < workers as u64
                        {
                            thread::sleep(Duration::from_millis(100));
                            inner.emit_progress(
                                &event_tx,
                                id,
                                uploaded.load(Ordering::Relaxed),
                                total,
                            );
                        }
                    })
                    .ok();
            }

            let mut handles = Vec::new();
            for (i, sess) in helpers.iter().enumerate() {
                let (start, len) = slice_range(total, workers, i + 1);
                let (l, r, up, cn, dn, dn_spawn_err) = (
                    local.clone(),
                    part.clone(),
                    uploaded.clone(),
                    cancel.clone(),
                    done.clone(),
                    done.clone(),
                );
                let sess = sess.clone();
                match std::thread::Builder::new()
                    .name(format!("sftp-slice-up-{i}"))
                    .spawn(move || {
                        let res = upload_slice(&sess.sftp(), &l, &r, start, len, &up, &cn);
                        dn.fetch_add(1, Ordering::Relaxed);
                        res
                    }) {
                    Ok(handle) => handles.push(handle),
                    Err(err) => {
                        dn_spawn_err.fetch_add(1, Ordering::Relaxed);
                        if first_err.is_none() {
                            first_err = Some(format!("{err}"));
                        }
                    }
                }
            }

            let (main_start, main_len) = slice_range(total, workers, 0);
            let main_res = upload_slice(
                &lane, &local, &part, main_start, main_len, &uploaded, &cancel,
            );
            done.fetch_add(1, Ordering::Relaxed);

            // Merge in the spawn failures collected above: a range that
            // was never written must fail the transfer rather than report
            // a complete file with a hole in it.
            let mut first_err = first_err.or_else(|| main_res.err().map(|e| format!("{e:#}")));
            for h in handles {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        if first_err.is_none() {
                            first_err = Some(format!("{e:#}"));
                        }
                    }
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some("slice thread panicked".into());
                        }
                    }
                }
            }
            ticker_done.store(true, Ordering::Relaxed);
            match first_err {
                Some(err) => {
                    // A slice failure usually means one of the helper
                    // connections is gone; drop the pool so the next
                    // transfer does not keep reusing a dead session.
                    inner.invalidate_helpers();
                    // Slice writes are not a contiguous prefix, so a part
                    // file left behind here would make a later resume
                    // append on top of a hole.  Drop it and let that
                    // resume start over instead.
                    smol::block_on(async { lane.remove_file(&part).await.ok() });
                    let failure = if cancel.load(Ordering::Relaxed) {
                        SliceFailure::Cancelled
                    } else {
                        SliceFailure::Failed(err)
                    };
                    let _ = done_tx.send(Err(failure));
                }
                None => {
                    // The mode came in with the OPEN, so one rename is all
                    // that is left to publish the file.
                    let published =
                        smol::block_on(async { publish_remote(&lane, &part, &remote).await });
                    match published {
                        Ok(()) => {
                            inner.emit_progress(&event_tx, id, total, total);
                            let _ = done_tx.send(Ok(()));
                        }
                        Err(err) => {
                            let _ = done_tx.send(Err(SliceFailure::Failed(format!("{err:#}"))));
                        }
                    }
                }
            }
        });
    match spawned {
        Ok(_) => match done_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(SliceFailure::Cancelled)) => Err(TransferError::Cancelled),
            Ok(Err(SliceFailure::Failed(msg))) => Err(TransferError::Failed(anyhow::anyhow!(msg))),
            Err(_) => Err(TransferError::Failed(anyhow::anyhow!(
                "parallel upload orchestrator dropped"
            ))),
        },
        Err(err) => Err(TransferError::Failed(anyhow::anyhow!(err))),
    }
}

fn upload_slice(
    sftp: &Sftp,
    local: &Path,
    remote: &Utf8PathBuf,
    start: u64,
    len: u64,
    uploaded: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<(), TransferError> {
    smol::block_on(async {
        let mut reader = smol::fs::File::open(local)
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        reader
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        let mut writer = sftp
            .open_with_mode(
                remote,
                wezterm_ssh::OpenOptions {
                    read: false,
                    write: Some(wezterm_ssh::WriteMode::WriteNoTruncate),
                    mode: 0,
                    ty: wezterm_ssh::OpenFileType::File,
                },
            )
            .await
            .map_err(TransferError::from)?;
        writer.seek(start).await.map_err(TransferError::from)?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut remaining = len;
        while remaining > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(TransferError::Cancelled);
            }
            let n = remaining.min(buf.len() as u64) as usize;
            with_io_timeout(reader.read_exact(&mut buf[..n]), || {
                std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
            })
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
            with_io_timeout(writer.write_all(&buf[..n]), || {
                std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
            })
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
            uploaded.fetch_add(n as u64, Ordering::Relaxed);
            remaining -= n as u64;
        }
        writer
            .flush()
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        writer
            .close()
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        Ok(())
    })
}

// One worker per slice plus the job's identity and its channel:
// a struct would only move these fields around.
#[allow(clippy::too_many_arguments)]
fn run_download_parallel(
    inner: Arc<TransferInner>,
    lane: Sftp,
    remote: Utf8PathBuf,
    local: PathBuf,
    total: u64,
    id: TransferId,
    event_tx: Sender<TransferEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<(), TransferError> {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), SliceFailure>>();
    let spawned = std::thread::Builder::new()
        .name("sftp-parallel-download".into())
        .spawn(move || {
            let part = local_part(&local);
            // A recursive download walks into directories that do not
            // exist locally yet; the sequential path does the same.
            if let Some(dir) = part.parent() {
                if let Err(err) = std::fs::create_dir_all(dir) {
                    let _ = done_tx.send(Err(SliceFailure::Failed(err.to_string())));
                    return;
                }
            }
            // Pre-allocate the part file so slices can seek-write into it.
            // A failure or cancel now leaves the destination untouched.
            if let Err(err) = std::fs::File::create(&part).and_then(|f| f.set_len(total)) {
                let _ = done_tx.send(Err(SliceFailure::Failed(err.to_string())));
                return;
            }

            // Report the file as started before connecting helpers, so a
            // slow auxiliary handshake does not look like a frozen queue.
            inner.emit_progress(&event_tx, id, 0, total);
            let helpers = inner.ensure_helpers();
            let workers = helpers.len() + 1;
            let uploaded = Arc::new(AtomicU64::new(0));
            let done = Arc::new(AtomicU64::new(0));
            let mut first_err: Option<String> = None;

            let ticker_done = Arc::new(AtomicBool::new(false));
            {
                let inner = inner.clone();
                let uploaded = uploaded.clone();
                let done = done.clone();
                let ticker_done = ticker_done.clone();
                let event_tx = event_tx.clone();
                std::thread::Builder::new()
                    .name("sftp-parallel-progress".into())
                    .spawn(move || {
                        // `ticker_done` is the exit that always happens:
                        // a panicking slice thread never bumps `done`.
                        while !ticker_done.load(Ordering::Relaxed)
                            && done.load(Ordering::Relaxed) < workers as u64
                        {
                            thread::sleep(Duration::from_millis(100));
                            inner.emit_progress(
                                &event_tx,
                                id,
                                uploaded.load(Ordering::Relaxed),
                                total,
                            );
                        }
                    })
                    .ok();
            }

            let mut handles = Vec::new();
            for (i, sess) in helpers.iter().enumerate() {
                let (start, len) = slice_range(total, workers, i + 1);
                let (r, l, up, cn, dn, dn_spawn_err) = (
                    remote.clone(),
                    part.clone(),
                    uploaded.clone(),
                    cancel.clone(),
                    done.clone(),
                    done.clone(),
                );
                let sess = sess.clone();
                match std::thread::Builder::new()
                    .name(format!("sftp-slice-down-{i}"))
                    .spawn(move || {
                        let res = download_slice(&sess.sftp(), &r, &l, start, len, &up, &cn);
                        dn.fetch_add(1, Ordering::Relaxed);
                        res
                    }) {
                    Ok(handle) => handles.push(handle),
                    Err(err) => {
                        dn_spawn_err.fetch_add(1, Ordering::Relaxed);
                        if first_err.is_none() {
                            first_err = Some(format!("{err}"));
                        }
                    }
                }
            }

            let (main_start, main_len) = slice_range(total, workers, 0);
            let main_res = download_slice(
                &lane, &remote, &part, main_start, main_len, &uploaded, &cancel,
            );
            done.fetch_add(1, Ordering::Relaxed);

            // Merge in the spawn failures collected above: a range that
            // was never written must fail the transfer rather than report
            // a complete file with a hole in it.
            let mut first_err = first_err.or_else(|| main_res.err().map(|e| format!("{e:#}")));
            for h in handles {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        if first_err.is_none() {
                            first_err = Some(format!("{e:#}"));
                        }
                    }
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some("slice thread panicked".into());
                        }
                    }
                }
            }
            ticker_done.store(true, Ordering::Relaxed);
            match first_err {
                Some(err) => {
                    // A slice failure usually means one of the helper
                    // connections is gone; drop the pool so the next
                    // transfer does not keep reusing a dead session.
                    inner.invalidate_helpers();
                    // The pre-allocated part file is holey, so it can
                    // never be resumed from; drop it rather than let a
                    // later resume trust its length.
                    std::fs::remove_file(&part).ok();
                    let failure = if cancel.load(Ordering::Relaxed) {
                        SliceFailure::Cancelled
                    } else {
                        SliceFailure::Failed(err)
                    };
                    let _ = done_tx.send(Err(failure));
                }
                None => {
                    let published = smol::block_on(async {
                        // Preserve the remote execute bits, the way the
                        // single-stream path does.
                        if let Ok(meta) = lane.metadata(&remote).await {
                            if let Some(perms) = meta.permissions {
                                let mode = perms.to_unix_mode() & 0o777;
                                if mode != 0 {
                                    std::fs::set_permissions(
                                        &part,
                                        std::fs::Permissions::from_mode(mode),
                                    )
                                    .ok();
                                }
                            }
                        }
                        publish_local(&part, &local)
                    });
                    match published {
                        Ok(()) => {
                            inner.emit_progress(&event_tx, id, total, total);
                            let _ = done_tx.send(Ok(()));
                        }
                        Err(err) => {
                            let _ = done_tx.send(Err(SliceFailure::Failed(format!("{err:#}"))));
                        }
                    }
                }
            }
        });
    match spawned {
        Ok(_) => match done_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(SliceFailure::Cancelled)) => Err(TransferError::Cancelled),
            Ok(Err(SliceFailure::Failed(msg))) => Err(TransferError::Failed(anyhow::anyhow!(msg))),
            Err(_) => Err(TransferError::Failed(anyhow::anyhow!(
                "parallel download orchestrator dropped"
            ))),
        },
        Err(err) => Err(TransferError::Failed(anyhow::anyhow!(err))),
    }
}

fn download_slice(
    sftp: &Sftp,
    remote: &Utf8PathBuf,
    local: &Path,
    start: u64,
    len: u64,
    uploaded: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<(), TransferError> {
    smol::block_on(async {
        let mut reader = sftp.open(remote).await.map_err(TransferError::from)?;
        reader.seek(start).await.map_err(TransferError::from)?;
        let mut writer = smol::fs::OpenOptions::new()
            .write(true)
            .open(local)
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        writer
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut remaining = len;
        while remaining > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(TransferError::Cancelled);
            }
            let n = remaining.min(buf.len() as u64) as usize;
            with_io_timeout(reader.read_exact(&mut buf[..n]), || {
                std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
            })
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
            with_io_timeout(writer.write_all(&buf[..n]), || {
                std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
            })
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
            uploaded.fetch_add(n as u64, Ordering::Relaxed);
            remaining -= n as u64;
        }
        writer
            .flush()
            .await
            .map_err(|e| TransferError::Failed(anyhow::anyhow!(e)))?;
        Ok(())
    })
}

/// Stream `reader` into `writer` in [CHUNK_SIZE] chunks, calling
/// `on_progress` with the cumulative byte count at most once per
/// [PROGRESS_INTERVAL], plus once with the final count.  Shared by the
/// transfer engine and by the single-shot copies (remote open preview,
/// editor write-back) so every path moves data in the same packet size.
pub(crate) async fn copy_stream<R, W>(
    reader: &mut R,
    writer: &mut W,
    start: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), TransferError>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut done: u64 = start;
    let mut last_emit = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(TransferError::Cancelled);
        }
        let n = with_io_timeout(reader.read(&mut buf), || {
            std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
        })
        .await?;
        if n == 0 {
            break;
        }
        with_io_timeout(writer.write_all(&buf[..n]), || {
            std::io::Error::new(std::io::ErrorKind::TimedOut, IO_STALLED)
        })
        .await?;
        done += n as u64;
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            on_progress(done);
            last_emit = Instant::now();
        }
    }
    on_progress(done);
    Ok(())
}

/// Human-readable byte count, e.g. `1.4 MB`.
/// Copy (or move, when `cut`) a local file or tree to a local path.
///
/// The SFTP engine has no local-to-local direction: yanking inside the
/// local panel used to be handed to `copy_remote`, which tried to open
/// the local paths over ssh and failed.  This is the local twin.
///
/// An existing destination file is skipped rather than replaced, so a
/// paste can never destroy a file the user did not mean to touch; the
/// returned count of skipped files lets the caller say so.
pub fn copy_local(from: &str, to: &str, cut: bool) -> Result<(u64, u64), String> {
    let from_path = std::path::Path::new(from);
    let to_path = std::path::Path::new(to);
    let meta = std::fs::symlink_metadata(from_path).map_err(|e| format!("{from}: {e}"))?;
    let (copied, skipped) = if meta.is_dir() {
        copy_local_tree(from_path, to_path)?
    } else {
        copy_local_file(from_path, to_path)?
    };
    if cut {
        let removed = if meta.is_dir() {
            std::fs::remove_dir_all(from_path)
        } else {
            std::fs::remove_file(from_path)
        };
        removed.map_err(|e| format!("{from}: {e}"))?;
    }
    Ok((copied, skipped))
}

fn copy_local_file(from: &std::path::Path, to: &std::path::Path) -> Result<(u64, u64), String> {
    if to.exists() {
        return Ok((0, 1));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::copy(from, to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok((1, 0))
}

/// Walk `from` and mirror it under `to`, skipping files that are there.
fn copy_local_tree(from: &std::path::Path, to: &std::path::Path) -> Result<(u64, u64), String> {
    let mut copied = 0u64;
    let mut skipped = 0u64;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    while let Some((src_dir, dst_dir)) = stack.pop() {
        let entries =
            std::fs::read_dir(&src_dir).map_err(|e| format!("{}: {e}", src_dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let src = entry.path();
            let dst = dst_dir.join(entry.file_name());
            let file_type = entry.file_type().map_err(|e| e.to_string())?;
            if file_type.is_dir() {
                stack.push((src, dst));
            } else if file_type.is_symlink() {
                // Copy what the link points at, like `cp -L`.
                let resolved = std::fs::canonicalize(&src).unwrap_or(src.clone());
                let (c, s) = copy_local_file(&resolved, &dst)?;
                copied += c;
                skipped += s;
            } else {
                let (c, s) = copy_local_file(&src, &dst)?;
                copied += c;
                skipped += s;
            }
        }
    }
    Ok((copied, skipped))
}

/// Create an empty file, or a directory, on this machine.  `create_new`
/// refuses an existing name instead of truncating it, so a typo cannot
/// silently empty a file.
pub fn create_local(path: &str, is_dir: bool) -> Result<(), String> {
    if is_dir {
        return std::fs::create_dir(path).map_err(|e| e.to_string());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The same on the remote side.  SFTP has no "create exclusively", so the
/// presence check comes first: without it `sftp.create` would truncate an
/// existing file to zero bytes.
pub async fn create_remote(
    sftp: &wezterm_ssh::Sftp,
    path: &str,
    is_dir: bool,
) -> Result<(), String> {
    if is_dir {
        return sftp
            .create_dir(path, 0o755)
            .await
            .map_err(|e| e.to_string());
    }
    if sftp.metadata(path).await.is_ok() {
        return Err(format!("{path}: already exists"));
    }
    let mut file = sftp.create(path).await.map_err(|e| e.to_string())?;
    file.close().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod local_copy_tests {
    use super::copy_local;

    fn tmpdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaku-copy-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Yank-and-paste inside the local panel has to copy locally, and a
    /// paste must never replace a file that is already there.
    #[test]
    fn local_copy_moves_and_skips_existing_files() {
        let dir = tmpdir();
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("nested/b.txt"), b"b").unwrap();
        let dst = dir.join("dst");

        // Directory copy, then a second copy that must skip everything.
        let (copied, skipped) = copy_local(
            &src.display().to_string(),
            &dst.display().to_string(),
            false,
        )
        .unwrap();
        assert_eq!((copied, skipped), (2, 0));
        assert_eq!(std::fs::read(dst.join("nested/b.txt")).unwrap(), b"b");
        let (_, skipped) = copy_local(
            &src.display().to_string(),
            &dst.display().to_string(),
            false,
        )
        .unwrap();
        assert_eq!(skipped, 2);
        assert!(src.join("a.txt").exists(), "copy removed the source");

        // Cut moves the tree and leaves nothing behind.
        let moved = dir.join("moved");
        copy_local(
            &src.display().to_string(),
            &moved.display().to_string(),
            true,
        )
        .unwrap();
        assert!(!src.exists(), "cut left the source behind");
        assert!(moved.join("nested/b.txt").exists());

        // A single existing destination file is left alone.
        let single = dir.join("single.txt");
        std::fs::write(&single, b"keep").unwrap();
        let (copied, skipped) = copy_local(
            &single.display().to_string(),
            &single.display().to_string(),
            false,
        )
        .unwrap();
        assert_eq!((copied, skipped), (0, 1));
        assert_eq!(std::fs::read(&single).unwrap(), b"keep");
        std::fs::remove_dir_all(&dir).ok();
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Human-readable throughput, e.g. `2.3 MB/s`.
pub fn format_speed(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes_per_sec;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes_per_sec:.0} B/s")
    } else {
        format!("{value:.1} {}/s", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::AsyncWrite;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn cancel_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn slice_ranges_tile_the_file_without_gaps() {
        for (total, workers) in [
            (0u64, 1usize),
            (1, 1),
            (1, 4),
            (1000, 3),
            (16 * 1024 * 1024, 4),
            (16 * 1024 * 1024 + 1, 4),
        ] {
            let covered: Vec<(u64, u64)> = (0..workers)
                .map(|i| {
                    let (start, len) = slice_range(total, workers, i);
                    (start, start + len)
                })
                .collect();
            assert_eq!(covered[0].0, 0, "first slice must start at 0");
            for pair in covered.windows(2) {
                assert_eq!(pair[0].1, pair[1].0, "gap or overlap in {total}/{workers}");
            }
            assert_eq!(
                covered.last().unwrap().1,
                total,
                "last slice must reach the end of {total}/{workers}"
            );
        }
    }

    #[test]
    fn part_files_sit_next_to_their_destination() {
        assert_eq!(
            local_part(Path::new("/tmp/a.bin")),
            PathBuf::from("/tmp/.kaku-part.a.bin")
        );
        assert_eq!(
            remote_part(&Utf8PathBuf::from("/root/a.bin")),
            Utf8PathBuf::from("/root/.kaku-part.a.bin")
        );
        // A relative destination with no directory component still gets
        // a sibling part file rather than landing somewhere else.
        assert_eq!(
            remote_part(&Utf8PathBuf::from("a.bin")),
            Utf8PathBuf::from(".kaku-part.a.bin")
        );
        // A nested destination keeps the part next to the file, so the
        // publish rename never crosses directories and creating the
        // destination's parent directory also covers the part file.
        assert_eq!(
            remote_part(&Utf8PathBuf::from("/root/sub/a.bin")),
            Utf8PathBuf::from("/root/sub/.kaku-part.a.bin")
        );
        assert_eq!(
            local_part(Path::new("/tmp/sub/a.bin"))
                .parent()
                .map(|p| p.to_path_buf()),
            Some(PathBuf::from("/tmp/sub"))
        );
        assert_eq!(
            remote_part(&Utf8PathBuf::from("/root/sub/a.bin"))
                .parent()
                .map(|p| p.to_path_buf()),
            Some(Utf8PathBuf::from("/root/sub"))
        );
    }

    /// In-memory AsyncWrite sink used to observe copy_stream output.
    #[derive(Default)]
    struct Sink(Vec<u8>);

    impl AsyncWrite for Sink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// AsyncWrite sink that always fails, for error propagation checks.
    struct FailingSink;

    impl AsyncWrite for FailingSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("sink unavailable")))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn copy_stream_moves_all_data_and_reports_final_progress() {
        smol::block_on(async {
            let payload: Vec<u8> = (0..(CHUNK_SIZE as u32 * 3 + 123))
                .map(|i| (i % 251) as u8)
                .collect();
            let mut sink = Sink::default();
            let cancel = cancel_flag();
            let mut seen = Vec::new();

            copy_stream(&mut &payload[..], &mut sink, 0, &cancel, &mut |bytes| {
                seen.push(bytes)
            })
            .await
            .unwrap();

            assert_eq!(sink.0.len(), payload.len());
            assert_eq!(sink.0, payload);
            assert_eq!(seen.last(), Some(&(payload.len() as u64)));
        });
    }

    #[test]
    fn copy_stream_stops_when_cancelled() {
        smol::block_on(async {
            let mut sink = Sink::default();
            let cancel = cancel_flag();
            let mut seen = Vec::new();

            // Raises the cancel flag after two reads so the check at the
            // top of the next copy loop iteration fires deterministically.
            struct CancelAfterReads {
                reads: usize,
                cancel: Arc<AtomicBool>,
            }
            impl smol::io::AsyncRead for CancelAfterReads {
                fn poll_read(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                    buf: &mut [u8],
                ) -> Poll<io::Result<usize>> {
                    let this = &mut *self;
                    this.reads += 1;
                    if this.reads >= 2 {
                        this.cancel.store(true, Ordering::Relaxed);
                    }
                    let n = std::cmp::min(buf.len(), 16);
                    buf[..n].fill(7);
                    Poll::Ready(Ok(n))
                }
            }

            let mut reader = CancelAfterReads {
                reads: 0,
                cancel: cancel.clone(),
            };
            let err = copy_stream(&mut reader, &mut sink, 0, &cancel, &mut |b| seen.push(b))
                .await
                .unwrap_err();
            assert!(matches!(err, TransferError::Cancelled));
            // Two 16-byte reads landed before the cancel took effect.
            assert_eq!(sink.0.len(), 32);
            assert!(sink.0.iter().all(|&b| b == 7));
            // Mid-loop progress was throttled away and the cancelled
            // return skips the final emit; the terminal state reports it.
            assert!(seen.is_empty());
        });
    }

    #[test]
    fn copy_stream_propagates_write_errors() {
        smol::block_on(async {
            let payload = b"hello".to_vec();
            let cancel = cancel_flag();
            let result = copy_stream(&mut &payload[..], &mut FailingSink, 0, &cancel, &mut |_| {})
                .await
                .unwrap_err();
            assert!(matches!(result, TransferError::Failed(_)));
        });
    }

    #[test]
    fn format_bytes_uses_sensible_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 + 400 * 1024), "1.4 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn format_speed_uses_sensible_units() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(999.0), "999 B/s");
        assert_eq!(format_speed(1024.0), "1.0 KB/s");
        assert_eq!(format_speed(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
    }
}
