//! State for the SFTP dual-pane overlay: panels, background operations,
//! and the shared transfer manager.

use super::types::{sort_entries, FileEntry, PanelSide, SftpPalette};
use crate::sftp_edit_sync::RemoteStamp;
use crate::sftp_transfer::TransferManager;
use smol::io::AsyncWriteExt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender as StdSender;
use std::sync::{Arc, Mutex};
use wezterm_ssh::Session;

/// A mutable directory view for one half of the dual pane.
pub(crate) struct Panel {
    /// Absolute path currently listed.
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// The last listing, before the text filter narrowed it.
    pub all_entries: Vec<FileEntry>,
    /// Case-insensitive substring filter over the listing.
    pub filter: Option<String>,
    /// Directories this panel has landed in, for back/forward movement.
    pub visited: Vec<String>,
    pub visited_pos: usize,
    /// Cursor index into `entries`.
    pub cursor: usize,
    /// First visible row index into `entries`.
    pub offset: usize,
    /// Indices into `entries` selected for bulk transfer.
    pub marked: HashSet<usize>,
    pub show_hidden: bool,
}

impl Panel {
    pub fn new(path: String) -> Self {
        Self {
            path,
            entries: Vec::new(),
            all_entries: Vec::new(),
            filter: None,
            visited: Vec::new(),
            visited_pos: 0,
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            show_hidden: false,
        }
    }

    pub fn title(&self) -> String {
        shorten_path(&self.path, 30)
    }

    pub fn set_entries(&mut self, all_entries: Vec<FileEntry>) {
        let mut kept: Vec<FileEntry> = all_entries
            .into_iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .collect();
        sort_entries(&mut kept);
        self.all_entries = kept;
        self.apply_filter();
        self.marked.clear();
        self.cursor = 0;
        self.offset = 0;
    }

    /// Narrow the listing to entries whose name contains `filter`.
    pub fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter.filter(|f| !f.is_empty());
        self.apply_filter();
        self.cursor = 0;
        self.offset = 0;
    }

    /// Recompute `entries` from `all_entries` and the active filter.
    pub fn apply_filter(&mut self) {
        self.entries = match self.filter.as_deref().map(str::to_lowercase) {
            Some(needle) => self
                .all_entries
                .iter()
                .filter(|entry| entry.name.to_lowercase().contains(&needle))
                .cloned()
                .collect(),
            None => self.all_entries.clone(),
        };
    }

    /// Remember a directory the panel actually landed in.
    pub fn record_visit(&mut self, path: &str) {
        if self.visited.get(self.visited_pos).map(String::as_str) == Some(path) {
            return;
        }
        self.visited.truncate(self.visited_pos + 1);
        self.visited.push(path.to_string());
        self.visited_pos = self.visited.len() - 1;
    }

    /// Directory to return to for `H`; None at the start of the history.
    pub fn back(&mut self) -> Option<String> {
        if self.visited_pos == 0 {
            return None;
        }
        self.visited_pos -= 1;
        self.visited.get(self.visited_pos).cloned()
    }

    /// Directory to move on to for `L`; None at the end of the history.
    pub fn forward(&mut self) -> Option<String> {
        if self.visited_pos + 1 >= self.visited.len() {
            return None;
        }
        self.visited_pos += 1;
        self.visited.get(self.visited_pos).cloned()
    }

    /// Toggle hidden files.  The caller refetches the listing so the
    /// filter applies to fresh data.
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
    }

    pub fn move_cursor(&mut self, delta: i32, visible_rows: usize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        let next = (self.cursor as i64 + delta as i64).clamp(0, len as i64 - 1) as usize;
        self.cursor = next;
        self.clamp_offset(visible_rows);
    }

    pub fn jump_to(&mut self, idx: usize, visible_rows: usize) {
        self.cursor = idx.min(self.entries.len().saturating_sub(1));
        self.clamp_offset(visible_rows);
    }

    /// Scroll offset follows the cursor the way list views do: the
    /// cursor never leaves the window while scrolling stays stable.
    pub fn clamp_offset(&mut self, visible_rows: usize) {
        let rows = visible_rows.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
    }

    pub fn current_entry(&self) -> Option<&FileEntry> {
        self.entries.get(self.cursor)
    }

    /// Entries covered by an action: the marked set when non-empty,
    /// otherwise the entry under the cursor.
    pub fn action_targets(&self) -> Vec<(usize, FileEntry)> {
        if self.marked.is_empty() {
            return self
                .current_entry()
                .map(|e| vec![(self.cursor, e.clone())])
                .unwrap_or_default();
        }
        let mut idxs: Vec<usize> = self.marked.iter().copied().collect();
        idxs.sort_unstable();
        idxs.into_iter()
            .filter_map(|i| self.entries.get(i).map(|e| (i, e.clone())))
            .collect()
    }
}

/// Results of background work, delivered to the overlay loop.
pub(crate) enum OpResult {
    Connected {
        session: Session,
        home: String,
    },
    ConnectFailed(String),
    /// The ssh handshake needs a password (or keyboard-interactive
    /// answer) from the user; reply with `Some(password)` to proceed
    /// or `None` to cancel authentication.
    NeedsAuth {
        username: String,
        prompt: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    /// The host is not in known_hosts; reply with the user's
    /// trust decision.
    NeedsHostVerify {
        message: String,
        reply: std::sync::mpsc::Sender<bool>,
    },
    Listed {
        side: PanelSide,
        path: String,
        entries: Result<Vec<FileEntry>, String>,
    },
    Mutated {
        side: PanelSide,
        outcome: Result<(), String>,
    },
    Opened {
        source: String,
        dest: PathBuf,
        outcome: Result<u64, String>,
        /// Remote stamp taken while downloading, so the write-back
        /// watcher can tell its own writes from someone else's.
        stamp: Option<RemoteStamp>,
    },
    Scanned {
        side: PanelSide,
        root: String,
        dest: String,
        dest_side: PanelSide,
        cut_source: bool,
        files: Result<Vec<ScanFile>, String>,
    },
}

/// One file discovered by a recursive tree scan.
#[derive(Clone, Debug)]
pub(crate) struct ScanFile {
    /// Absolute path on its side (local path or remote path).
    pub abs: String,
    /// Path relative to the scan root.
    pub rel: String,
    pub size: u64,
}

/// A folder waiting for the recursive-transfer confirmation.
#[derive(Clone, Debug)]
pub(crate) struct ScanRequest {
    /// Side holding the folder.
    pub side: PanelSide,
    /// Absolute path of the folder being copied.
    pub root: String,
    /// Directory the contents land in.
    pub dest: String,
    /// Side holding that directory.
    pub dest_side: PanelSide,
    /// Remove the source tree once the copy lands (a cut).
    pub cut_source: bool,
}

/// A background job request handed to the worker thread.
pub(crate) enum OpRequest {
    Ls {
        side: PanelSide,
        path: String,
    },
    /// Recursively enumerate files under `root` on `side`, to be written
    /// under `dest` on `dest_side` (the two sides differ for F5, and are
    /// the same for pasting a copied folder).
    ScanTree {
        side: PanelSide,
        root: String,
        dest: String,
        dest_side: PanelSide,
        cut_source: bool,
    },
    /// Download a remote file to `dest` using the worker's current
    /// session, so opening a file never rides a stale transfer channel.
    OpenRemote {
        source: String,
        dest: PathBuf,
    },
    Rename {
        side: PanelSide,
        from: String,
        to: String,
    },
    Mkdir {
        side: PanelSide,
        path: String,
    },
    Delete {
        side: PanelSide,
        path: String,
        is_dir: bool,
    },
}

/// Pending interactive answer channels owned by the overlay while an
/// [InputMode::Password] or [InputMode::ConfirmHostKey] prompt is on
/// screen.
pub(crate) enum AuthReply {
    Password(std::sync::mpsc::Sender<Option<String>>),
    HostVerify(std::sync::mpsc::Sender<bool>),
}

/// Input-line modes overlaying the status row.
#[derive(Clone, Debug)]
pub(crate) enum InputMode {
    Mkdir,
    Rename {
        original: String,
    },
    ConfirmDelete {
        names: Vec<String>,
    },
    /// Masked password entry for the ssh handshake.
    Password {
        username: String,
        prompt: String,
    },
    /// Unknown-host trust decision with the fingerprint message.
    ConfirmHostKey {
        message: String,
    },
    /// A queued transfer hits an existing destination; the user picks
    /// overwrite / skip / resume, or applies a decision to all.
    ConfirmOverwrite,
    /// Recursive folder transfer confirmation before scanning.
    ConfirmFolder {
        side: PanelSide,
        name: String,
    },
    /// Live substring filter over the current listing (`/` or `f`).
    Filter,
    /// Type a path to jump to (`z`).
    JumpTo,
}

/// Paths copied or cut in the browser, waiting for a paste.
#[derive(Clone, Debug)]
pub(crate) struct Clipboard {
    pub side: PanelSide,
    /// (absolute path, is directory, size)
    pub items: Vec<(String, bool, u64)>,
    pub cut: bool,
}

/// A transfer waiting for an overwrite decision.
#[derive(Clone, Debug)]
pub(crate) struct PendingTransfer {
    pub direction: crate::sftp_transfer::Direction,
    pub source: String,
    pub dest: String,
    /// Size of the source file.
    pub size: u64,
    /// Size of the existing destination file, if any.
    pub dest_size: Option<u64>,
    /// The source is removed once this transfer lands (a cut).
    pub cut_source: bool,
}

impl PendingTransfer {
    /// A partial destination can be completed instead of restarted.
    pub fn resumable(&self) -> bool {
        self.dest_size.map(|d| d < self.size).unwrap_or(false)
    }
}

pub(crate) struct App {
    pub cols: usize,
    pub rows: usize,
    pub palette: SftpPalette,
    pub panels: [Panel; 2],
    pub focus: PanelSide,
    pub session: Option<Session>,
    pub transfers: Option<TransferManager>,
    /// Human-readable remote target, e.g. `user@host`.
    pub remote_label: String,
    pub connecting: Option<String>,
    pub message: Option<String>,
    pub error: Option<String>,
    pub input_mode: Option<InputMode>,
    pub input_line: String,
    /// Reply channel for the active auth/host-verify prompt.
    pub auth_reply: Option<AuthReply>,
    /// Last mouse click (side, entry index, instant) for double-click
    /// detection.
    pub last_click: Option<(PanelSide, usize, std::time::Instant)>,
    /// Transfers waiting for an overwrite decision.
    pub pending: Vec<PendingTransfer>,
    /// Folders awaiting a recursive-transfer confirmation (side, path).
    pub scan_queue: Vec<ScanRequest>,
    /// Applied-to-all overwrite decision: Some(true) = overwrite every
    /// remaining conflict, Some(false) = skip them all.
    pub overwrite_all: Option<bool>,
    /// Whether the left mouse button was held by the previous mouse event.
    /// A click is the false-to-true edge of this, so a drag (press, moves,
    /// release) counts once and the release never counts.
    pub mouse_left_down: bool,
    /// A download to the user's download folder: reveal it in Finder
    /// when the transfer with this id finishes.
    pub pending_reveal: Option<(u64, PathBuf)>,
    /// F1 key reference is on screen.
    pub help: bool,
    /// Paths yanked with `y`/`x`, waiting for `p`.
    pub clipboard: Option<Clipboard>,
    /// Transfers that should remove their source once they land (a cut).
    /// (transfer id, side, path, is_dir)
    pub cut_pending: Vec<(u64, PanelSide, String, bool)>,
    pub quit: bool,
    pub pending_g: bool,
}

impl App {
    pub fn new(cols: usize, rows: usize, palette: SftpPalette, local_path: String) -> Self {
        Self {
            cols,
            rows,
            palette,
            panels: [
                Panel::new(local_path),
                // Empty path: the first remote listing resolves it to
                // the remote home directory.
                Panel::new(String::new()),
            ],
            focus: PanelSide::Local,
            session: None,
            transfers: None,
            remote_label: String::new(),
            connecting: None,
            message: None,
            error: None,
            input_mode: None,
            input_line: String::new(),
            auth_reply: None,
            pending: Vec::new(),
            scan_queue: Vec::new(),
            overwrite_all: None,
            mouse_left_down: false,
            help: false,
            clipboard: None,
            cut_pending: Vec::new(),
            pending_reveal: None,
            last_click: None,
            quit: false,
            pending_g: false,
        }
    }

    pub fn panel(&self, side: PanelSide) -> &Panel {
        &self.panels[side as usize]
    }

    pub fn panel_mut(&mut self, side: PanelSide) -> &mut Panel {
        &mut self.panels[side as usize]
    }

    pub fn visible_rows(&self) -> usize {
        // Two border rows + title row per pane, plus a status row, a
        // transfer row, and an input row at the bottom.
        self.rows.saturating_sub(5).max(1)
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.error = None;
        self.message = Some(msg.into());
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.message = None;
        self.error = Some(msg.into());
    }
}

/// Compact a path for a pane title: keeps the first component and the
/// last three when the middle would overflow `max_width`.
pub(crate) fn shorten_path(path: &str, max_width: usize) -> String {
    if path.chars().count() <= max_width {
        return path.to_string();
    }
    let home = dirs_next::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if display.chars().count() <= max_width {
        return display;
    }
    let parts: Vec<&str> = display.split('/').collect();
    if parts.len() <= 4 {
        return display;
    }
    let tail = format!(
        "{}/{}/{}",
        parts[parts.len() - 3],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    );
    let mut shortened = format!("{}/…", parts[0]);
    if shortened.chars().count() + tail.chars().count() < max_width {
        shortened.push('/');
        shortened.push_str(&tail);
    }
    shortened
}

/// Spawn a worker that performs sftp/local operations off the overlay
/// thread.  Requests flow one way, results the other; the worker exits
/// when the request channel closes (the overlay loop owns the receiver)
/// or when the process ends.  `session_slot` is shared with the connect
/// thread so the session becomes available to fs ops the moment it
/// authenticates.
pub(crate) fn spawn_worker(
    session_slot: Arc<Mutex<Option<Session>>>,
    initial_request: OpRequest,
    tx: StdSender<OpResult>,
) -> anyhow::Result<std::sync::mpsc::Sender<OpRequest>> {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<OpRequest>();
    req_tx.send(initial_request)?;
    std::thread::Builder::new()
        .name("sftp-overlay-worker".into())
        .spawn(move || {
            for req in req_rx.iter() {
                match req {
                    OpRequest::OpenRemote { source, dest } => {
                        let result: Result<(u64, Option<RemoteStamp>), String> =
                            match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    let dest = dest.clone();
                                    let source = source.clone();
                                    let part = crate::sftp_transfer::local_part(&dest);
                                    with_timeout(REMOTE_OP_TIMEOUT, async move {
                                        let mut reader = sftp
                                            .open(source.as_str())
                                            .await
                                            .map_err(|e| e.to_string())?;
                                        let mut writer = smol::fs::File::create(&part)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                        let mut written = 0u64;
                                        let cancel = std::sync::atomic::AtomicBool::new(false);
                                        // Same 256KiB packets as the transfer
                                        // engine; opening a remote file is a
                                        // transfer like any other.
                                        crate::sftp_transfer::copy_stream(
                                            &mut reader,
                                            &mut writer,
                                            0,
                                            &cancel,
                                            &mut |bytes| written = bytes,
                                        )
                                        .await
                                        .map_err(|e| format!("{e:#}"))?;
                                        writer.flush().await.map_err(|e| e.to_string())?;
                                        writer.close().await.map_err(|e| e.to_string())?;
                                        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
                                        let stamp = sftp
                                            .metadata(source.as_str())
                                            .await
                                            .ok()
                                            .map(|m| crate::sftp_edit_sync::stamp_from(&m));
                                        Ok((written, stamp))
                                    })
                                    .unwrap_or_else(|| Err(OP_TIMED_OUT.to_string()))
                                }
                                None => Err("not connected".to_string()),
                            };
                        let (outcome, stamp) = match result {
                            Ok((bytes, stamp)) => (Ok(bytes), stamp),
                            Err(err) => (Err(err), None),
                        };
                        let _ = tx.send(OpResult::Opened {
                            source,
                            dest,
                            outcome,
                            stamp,
                        });
                    }
                    OpRequest::ScanTree {
                        side,
                        root,
                        dest,
                        dest_side,
                        cut_source,
                    } => {
                        let files = match side {
                            PanelSide::Local => scan_local_tree(&root),
                            PanelSide::Remote => match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    smol::block_on(async {
                                        let mut out = Vec::new();
                                        let result =
                                            scan_remote_tree(&sftp, &root, &mut out, 0).await;
                                        result.map(|_| out)
                                    })
                                }
                                None => Err("not connected".to_string()),
                            },
                        };
                        let _ = tx.send(OpResult::Scanned {
                            side,
                            root,
                            dest,
                            dest_side,
                            cut_source,
                            files,
                        });
                    }
                    OpRequest::Ls { side, path } => {
                        // An empty remote path resolves to the remote
                        // home directory via canonicalize(".").
                        let (path, entries) = match side {
                            PanelSide::Local => (path.clone(), list_local(&path)),
                            PanelSide::Remote => match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    with_timeout(REMOTE_OP_TIMEOUT, async {
                                        let resolved = if path.is_empty() {
                                            sftp.canonicalize(".")
                                                .await
                                                .map(|p| p.to_string())
                                                .unwrap_or_else(|_| "/".to_string())
                                        } else {
                                            path.clone()
                                        };
                                        let result = list_remote(&sftp, &resolved).await;
                                        (resolved, result)
                                    })
                                    .unwrap_or_else(|| {
                                        let fallback = if path.is_empty() {
                                            "/".to_string()
                                        } else {
                                            path.clone()
                                        };
                                        (fallback, Err(OP_TIMED_OUT.to_string()))
                                    })
                                }
                                None => (path, Err("not connected".to_string())),
                            },
                        };
                        let _ = tx.send(OpResult::Listed {
                            side,
                            path,
                            entries,
                        });
                    }
                    OpRequest::Rename { side, from, to } => {
                        let outcome = match side {
                            PanelSide::Local => {
                                std::fs::rename(&from, &to).map_err(|e| e.to_string())
                            }
                            PanelSide::Remote => match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    with_timeout(
                                        REMOTE_OP_TIMEOUT,
                                        sftp.rename(&from, &to, Default::default()),
                                    )
                                    .map(|r| r.map_err(|e| e.to_string()))
                                    .unwrap_or_else(|| Err(OP_TIMED_OUT.to_string()))
                                }
                                None => Err("not connected".to_string()),
                            },
                        };
                        let _ = tx.send(OpResult::Mutated { side, outcome });
                    }
                    OpRequest::Mkdir { side, path } => {
                        let outcome = match side {
                            PanelSide::Local => {
                                std::fs::create_dir(&path).map_err(|e| e.to_string())
                            }
                            PanelSide::Remote => match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    with_timeout(REMOTE_OP_TIMEOUT, sftp.create_dir(&path, 0o755))
                                        .map(|r| r.map_err(|e| e.to_string()))
                                        .unwrap_or_else(|| Err(OP_TIMED_OUT.to_string()))
                                }
                                None => Err("not connected".to_string()),
                            },
                        };
                        let _ = tx.send(OpResult::Mutated { side, outcome });
                    }
                    OpRequest::Delete { side, path, is_dir } => {
                        let outcome = match side {
                            PanelSide::Local => {
                                if is_dir {
                                    std::fs::remove_dir(&path).map_err(|e| e.to_string())
                                } else {
                                    std::fs::remove_file(&path).map_err(|e| e.to_string())
                                }
                            }
                            PanelSide::Remote => match session_slot.lock().unwrap().clone() {
                                Some(session) => {
                                    let sftp = session.sftp();
                                    with_timeout(REMOTE_OP_TIMEOUT, async move {
                                        if is_dir {
                                            sftp.remove_dir(&path).await
                                        } else {
                                            sftp.remove_file(&path).await
                                        }
                                        .map_err(|e| e.to_string())
                                    })
                                    .unwrap_or_else(|| Err(OP_TIMED_OUT.to_string()))
                                }
                                None => Err("not connected".to_string()),
                            },
                        };
                        let _ = tx.send(OpResult::Mutated { side, outcome });
                    }
                }
            }
        })?;
    Ok(req_tx)
}

/// How long to wait for the user to type a password or trust decision
/// before giving up on the connection.
const USER_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Hard cap on any single remote sftp operation.  A half-dead
/// connection (NAT drop, server restart) otherwise wedges the worker
/// forever with no error and no recovery.
pub(crate) const REMOTE_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub(crate) const OP_TIMED_OUT: &str = "connection timed out; session unresponsive";

/// Run an async sftp operation under a hard timeout.
pub(crate) fn with_timeout<T>(
    dur: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    smol::block_on(async {
        smol::future::or(async { Some(fut.await) }, async {
            smol::Timer::after(dur).await;
            None
        })
        .await
    })
}

/// Connect to `user@host` on a dedicated thread, surfacing password and
/// unknown-host prompts to the overlay through `tx`.  On success the
/// session lands in `session_slot` (shared with the fs-op worker) and an
/// [OpResult::Connected] is emitted.
pub(crate) fn spawn_connect(
    target: String,
    session_slot: Arc<Mutex<Option<Session>>>,
    tx: StdSender<OpResult>,
) {
    std::thread::Builder::new()
        .name("sftp-overlay-connect".into())
        .spawn(move || {
            if let Err(err) = connect_session(&target, &session_slot, &tx) {
                let _ = tx.send(OpResult::ConnectFailed(format!("{err:#}")));
            }
        })
        .ok();
}

/// Establish a fresh ssh session for a `user@host` target parsed from a
/// pane's ssh command line.  Reads ~/.ssh/config through wezterm-ssh.
/// Agent and key auth proceed silently; password and keyboard-
/// interactive prompts round-trip through the overlay input line, and an
/// unknown host key asks for an explicit trust decision.
fn connect_session(
    target: &str,
    session_slot: &Arc<Mutex<Option<Session>>>,
    tx: &StdSender<OpResult>,
) -> anyhow::Result<()> {
    use wezterm_ssh::SessionEvent;

    let (user, host) = match target.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, target.to_string()),
    };

    let mut cfg_map = crate::sftp_sessions::build_config_for_target(user.as_deref(), &host);
    if let Some(user) = user {
        cfg_map.insert("user".to_string(), user);
    }
    // Keep idle connections alive through NAT/server idle drops; the
    // libssh backend sends ignore packets every interval seconds.
    cfg_map.insert("serveraliveinterval".to_string(), "15".to_string());

    let (session, events) = Session::connect(cfg_map)?;
    // Bridge the smol receiver onto a std channel so the wait below
    // can time out without an async context.
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("sftp-connect-events".into())
        .spawn(move || {
            while let Ok(event) = smol::block_on(events.recv()) {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        })?;
    let mut auth_attempts = 0usize;
    // Password that authenticated this connection, remembered for other
    // connections to the same host for the rest of the app run.
    let mut working_password: Option<String> = None;
    loop {
        let event = event_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| anyhow::anyhow!("timed out connecting to {target}"))?;
        match event {
            SessionEvent::Authenticated => {
                if let Some(password) = &working_password {
                    crate::sftp_sessions::remember_password(target, password);
                }
                break;
            }
            SessionEvent::Banner(_) => {}
            SessionEvent::HostVerify(ev) => {
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                let _ = tx.send(OpResult::NeedsHostVerify {
                    message: ev.message.clone(),
                    reply: reply_tx,
                });
                let trust = reply_rx.recv_timeout(USER_PROMPT_TIMEOUT).unwrap_or(false);
                ev.try_answer(trust).ok();
            }
            SessionEvent::Authenticate(ev) => {
                auth_attempts += 1;
                // A password typed earlier in this app run answers
                // without another prompt; it was only removed from the
                // cache, so a rejection falls through to asking.
                let answered = match crate::sftp_sessions::take_password(target) {
                    Some(password) => Some(password),
                    None => {
                        let base_prompt = ev
                            .prompts
                            .first()
                            .map(|p| p.prompt.trim_end_matches(':').to_string())
                            .unwrap_or_else(|| "Password".to_string());
                        let prompt = if auth_attempts > 1 {
                            format!("{base_prompt} (attempt {auth_attempts})")
                        } else {
                            base_prompt
                        };
                        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                        let _ = tx.send(OpResult::NeedsAuth {
                            username: ev.username.clone(),
                            prompt,
                            reply: reply_tx,
                        });
                        // The overlay returns None when the user
                        // cancels; an empty vec then lets the handshake
                        // fail cleanly.
                        match reply_rx.recv_timeout(USER_PROMPT_TIMEOUT) {
                            Ok(Some(password)) => Some(password),
                            _ => None,
                        }
                    }
                };
                // Multi-prompt keyboard-interactive challenges are rare
                // for password servers; fill every prompt with the one
                // answer.
                let answers: Vec<String> = match &answered {
                    Some(password) => vec![password.clone(); ev.prompts.len().max(1)],
                    None => Vec::new(),
                };
                if answered.is_some() {
                    working_password = answered;
                }
                ev.try_answer(answers).ok();
            }
            SessionEvent::HostVerificationFailed(failed) => {
                crate::sftp_sessions::forget_password(target);
                return Err(anyhow::anyhow!(
                    "host key verification failed for {}: {}",
                    failed.remote_address,
                    failed.key
                ));
            }
            SessionEvent::Error(err) => {
                return Err(anyhow::anyhow!("ssh error: {}", err));
            }
        }
    }

    // Resolve the remote home directory so the remote panel can start
    // where the user lands on login.
    let sftp = session.sftp();
    let home = with_timeout(REMOTE_OP_TIMEOUT, sftp.canonicalize("."))
        .and_then(|r| r.ok())
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".to_string());
    *session_slot.lock().unwrap() = Some(session.clone());
    // Pool the authenticated handle so later SFTP toggles on the same
    // host never re-run the handshake.
    crate::sftp_sessions::insert(target, session.clone());
    let _ = tx.send(OpResult::Connected { session, home });
    Ok(())
}

pub(crate) fn list_local(path: &str) -> Result<Vec<FileEntry>, String> {
    let mut out = Vec::new();
    let read = std::fs::read_dir(path).map_err(|e| describe_local_error(path, &e))?;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(link_meta) = entry.metadata() else {
            continue;
        };
        // Follow symlinks so navigation and size reflect the target.
        let meta = std::fs::metadata(entry.path()).unwrap_or(link_meta);
        out.push(FileEntry {
            name,
            is_dir: meta.is_dir(),
            is_symlink: entry.file_type().map(|t| t.is_symlink()).unwrap_or(false),
            size: if meta.is_dir() { 0 } else { meta.len() },
            mode: Some(std::os::unix::fs::MetadataExt::mode(&meta) & 0o777),
        });
    }
    Ok(out)
}

/// macOS privacy denials arrive as a bare `Operation not permitted`
/// (os error 1) with no prompt, which reads like a filesystem bug.  Name
/// the cause and the fix instead: replacing an ad-hoc signed build
/// invalidates the folder grant recorded for the previous binary.
fn describe_local_error(path: &str, err: &std::io::Error) -> String {
    if err.raw_os_error() == Some(1) {
        format!(
            "{path}: macOS blocked access (Operation not permitted). \
             Grant it in System Settings > Privacy & Security > Files and Folders, \
             then retry; rebuilding the app (ad-hoc signature) asks again."
        )
    } else {
        format!("{path}: {err}")
    }
}

pub(crate) async fn list_remote(
    sftp: &wezterm_ssh::Sftp,
    path: &str,
) -> Result<Vec<FileEntry>, String> {
    let entries = sftp
        .read_dir(path)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    let mut out = Vec::new();
    for (full_path, meta) in entries {
        let is_symlink = meta.is_symlink();
        // Resolve symlink targets so directories remain navigable.
        let meta = if is_symlink {
            sftp.metadata(&full_path).await.unwrap_or(meta)
        } else {
            meta
        };
        let name = full_path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| full_path.to_string());
        out.push(FileEntry {
            name,
            is_dir: meta.is_dir(),
            is_symlink,
            size: meta.size.unwrap_or(0),
            mode: meta.permissions.map(|p| p.to_unix_mode() & 0o777),
        });
    }
    Ok(out)
}

/// Directory where remote files are downloaded before being opened
/// with the default macOS application.
pub(crate) fn open_cache_dir() -> PathBuf {
    let dir = dirs_next::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("kaku")
        .join("sftp-open");
    std::fs::create_dir_all(&dir).ok();
    dir
}

const SCAN_MAX_FILES: usize = 5000;
const SCAN_MAX_DEPTH: usize = 12;

pub(crate) fn scan_local_tree(root: &str) -> Result<Vec<ScanFile>, String> {
    let mut out = Vec::new();
    walk_local(Path::new(root), "", &mut out)?;
    Ok(out)
}

fn walk_local(dir: &Path, rel: &str, out: &mut Vec<ScanFile>) -> Result<(), String> {
    if out.len() >= SCAN_MAX_FILES {
        return Ok(());
    }
    let read = std::fs::read_dir(dir).map_err(|e| format!("{dir:?}: {e}"))?;
    for entry in read.flatten() {
        if out.len() >= SCAN_MAX_FILES {
            return Ok(());
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let child_rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if ft.is_dir() {
            walk_local(&entry.path(), &child_rel, out)?;
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(ScanFile {
                abs: entry.path().to_string_lossy().to_string(),
                rel: child_rel,
                size,
            });
        }
    }
    Ok(())
}

async fn scan_remote_tree(
    sftp: &wezterm_ssh::Sftp,
    root: &str,
    out: &mut Vec<ScanFile>,
    depth: usize,
) -> Result<(), String> {
    if depth > SCAN_MAX_DEPTH || out.len() >= SCAN_MAX_FILES {
        return Ok(());
    }
    let entries = sftp
        .read_dir(root)
        .await
        .map_err(|e| format!("{root}: {e}"))?;
    let root_prefix = if root.ends_with('/') {
        root.to_string()
    } else {
        format!("{root}/")
    };
    for (full, meta) in entries {
        if out.len() >= SCAN_MAX_FILES {
            return Ok(());
        }
        let full = full.to_string();
        let rel = full
            .strip_prefix(&root_prefix)
            .map(ToString::to_string)
            .unwrap_or_else(|| full.clone());
        // Directory symlinks stay unexplored to avoid loops; they are
        // treated as files (their listing entry still shows them).
        if meta.is_dir() && !meta.is_symlink() {
            Box::pin(scan_remote_tree(sftp, &full, out, depth + 1)).await?;
        } else {
            out.push(ScanFile {
                abs: full,
                rel,
                size: meta.size.unwrap_or(0),
            });
        }
    }
    Ok(())
}

/// Join a directory path and a name without duplicating slashes.
pub(crate) fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of a path, `/` for the root itself.
pub(crate) fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS denials arrive as a bare `Operation not permitted`; the
    /// panel has to name the fix, because the user sees no prompt.
    #[test]
    fn permission_denied_names_the_fix() {
        let err = std::io::Error::from_raw_os_error(1);
        let message = describe_local_error("/Users/me/Documents", &err);
        assert!(message.contains("System Settings"), "{}", message);
        assert!(message.contains("/Users/me/Documents"), "{}", message);
    }

    #[test]
    fn other_local_errors_keep_the_os_message() {
        let err = std::io::Error::from_raw_os_error(2);
        let message = describe_local_error("/nope", &err);
        assert!(message.contains("/nope"), "{}", message);
        assert!(!message.contains("System Settings"), "{}", message);
    }

    /// The filter narrows the listing without touching the server, so a
    /// second filter or a cleared one always sees the full set again.
    #[test]
    fn filter_narrows_and_restores_the_listing() {
        let mut panel = Panel::new("/tmp".to_string());
        panel.set_entries(vec![
            entry("alpha.txt", false),
            entry("beta.txt", false),
            entry("Gamma.md", false),
            entry("dir", true),
        ]);
        assert_eq!(panel.entries.len(), 4);

        panel.set_filter(Some("eta".to_string()));
        assert_eq!(names(&panel), vec!["beta.txt"]);

        // Case-insensitive, and matching anywhere in the name.
        panel.set_filter(Some("mma".to_string()));
        assert_eq!(names(&panel), vec!["Gamma.md"]);

        panel.set_filter(None);
        assert_eq!(panel.entries.len(), 4);
    }

    #[test]
    fn history_walks_back_and_forward() {
        let mut panel = Panel::new("/a".to_string());
        panel.record_visit("/a");
        panel.record_visit("/a/b");
        panel.record_visit("/a/b/c");

        assert_eq!(panel.back().as_deref(), Some("/a/b"));
        assert_eq!(panel.back().as_deref(), Some("/a"));
        assert_eq!(panel.back(), None);
        assert_eq!(panel.forward().as_deref(), Some("/a/b"));

        // Visiting somewhere new drops the forward branch.
        panel.record_visit("/a/x");
        assert_eq!(panel.forward(), None);
        assert_eq!(panel.back().as_deref(), Some("/a/b"));
    }

    fn entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: 1,
            mode: Some(0o644),
        }
    }

    fn names(panel: &Panel) -> Vec<String> {
        panel.entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn join_and_parent_paths_roundtrip() {
        assert_eq!(join_path("/home/u", "file"), "/home/u/file");
        assert_eq!(join_path("/", "file"), "/file");
        assert_eq!(parent_path("/home/u"), "/home");
        assert_eq!(parent_path("/home"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/home/u/"), "/home");
    }

    #[test]
    fn shorten_path_prefers_tilde_and_tail() {
        let home = dirs_next::home_dir().unwrap().to_string_lossy().to_string();
        let deep = format!("{}/a/b/c/d/e", home);
        let shortened = shorten_path(&deep, 20);
        assert!(shortened.starts_with("~/"));
        assert!(shortened.chars().count() <= 20);
    }

    #[test]
    fn panel_cursor_clamps_within_window() {
        let mut panel = Panel::new("/".into());
        panel.entries = (0..30)
            .map(|i| FileEntry {
                name: format!("f{i}"),
                is_dir: false,
                is_symlink: false,
                size: 0,
                mode: None,
            })
            .collect();
        panel.jump_to(29, 10);
        assert_eq!(panel.cursor, 29);
        assert_eq!(panel.offset, 20);
        panel.jump_to(0, 10);
        assert_eq!(panel.offset, 0);
    }

    #[test]
    fn action_targets_prefers_marks_over_cursor() {
        let mut panel = Panel::new("/".into());
        panel.entries = vec![
            FileEntry {
                name: "a".into(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                mode: None,
            },
            FileEntry {
                name: "b".into(),
                is_dir: false,
                is_symlink: false,
                size: 2,
                mode: None,
            },
        ];
        panel.cursor = 1;
        assert_eq!(panel.action_targets().len(), 1);
        assert_eq!(panel.action_targets()[0].1.name, "b");
        panel.marked.insert(0);
        assert_eq!(panel.action_targets().len(), 1);
        assert_eq!(panel.action_targets()[0].1.name, "a");
    }
}
