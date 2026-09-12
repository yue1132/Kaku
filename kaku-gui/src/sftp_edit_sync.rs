//! Watches local copies of remote files that were opened for editing
//! and syncs saved changes back to the server.
//!
//! Opening a remote file downloads it to the preview cache and opens
//! it with the default editor.  A watcher thread per file re-uploads
//! the content whenever the local copy settles after a change, so
//! editing in any macOS app round-trips to the server.

use smol::io::AsyncWriteExt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use wezterm_ssh::{Metadata, Session, Utf8PathBuf};

/// How many remote files may be watched at once.
const MAX_WATCHERS: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// After a change is detected, wait for the file to stay unchanged for
/// this long before uploading (editors and autosave write in steps).
const QUIET_PERIOD: Duration = Duration::from_millis(1500);
/// Delay before retrying a failed upload, doubled per consecutive
/// failure (5s, 10s, 20s, ...) so a server hiccup is ridden out without
/// hammering the connection.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(5000);
/// Upper bound for a single retry delay.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Consecutive failed syncs before this save is given up on.  A permanent
/// failure (read-only file, revoked permission, full disk) must stop
/// instead of retrying and notifying forever; the watcher keeps running,
/// so the user's next save starts a fresh budget and a transient outage
/// still recovers on its own.
const MAX_SYNC_ATTEMPTS: u32 = 5;

static ACTIVE_WATCHERS: AtomicUsize = AtomicUsize::new(0);

fn watched_sources() -> &'static Mutex<HashSet<String>> {
    static WATCHED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WATCHED.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    size: u64,
    mtime: u64,
}

fn stat_of(path: &Path) -> Option<Stat> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stat {
        size: meta.len(),
        mtime: meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    })
}

/// What the watch loop should do after observing the current file state.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Nothing to do.
    Idle,
    /// A change was just observed; start/refresh the quiet period.
    Changed,
    /// The file changed and has been quiet; upload it now.
    Upload,
    /// The local file is gone; stop watching.
    Stop,
}

/// State machine for one watched file, kept pure for testing.
struct SyncTracker {
    last_seen: Option<Stat>,
    last_synced: Option<Stat>,
    dirty: bool,
    changed_at: Option<Instant>,
    /// Failed sync attempts for the current dirty state; reset by the next
    /// local change and by a successful sync.
    attempts: u32,
    next_retry: Option<Instant>,
}

impl SyncTracker {
    fn new(initial: Option<Stat>) -> Self {
        Self {
            last_seen: initial,
            last_synced: initial,
            dirty: false,
            changed_at: None,
            attempts: 0,
            next_retry: None,
        }
    }

    fn observe(&mut self, current: Option<Stat>, now: Instant) -> Action {
        let Some(cur) = current else {
            return Action::Stop;
        };
        if Some(cur) != self.last_seen {
            self.last_seen = Some(cur);
            self.changed_at = Some(now);
            self.dirty = self.last_synced != Some(cur);
            // A new save is a new intent to write: retry it from scratch.
            self.attempts = 0;
            self.next_retry = None;
            return Action::Changed;
        }
        if self.dirty
            && self.attempts < MAX_SYNC_ATTEMPTS
            && self
                .changed_at
                .map(|t| now.duration_since(t) >= QUIET_PERIOD)
                .unwrap_or(false)
            && self.next_retry.map(|t| now >= t).unwrap_or(true)
        {
            return Action::Upload;
        }
        Action::Idle
    }

    fn mark_synced(&mut self, stat: Stat) {
        self.last_synced = Some(stat);
        self.dirty = false;
        self.attempts = 0;
        self.next_retry = None;
    }

    /// Record a failure and back off before the next attempt.  Returns
    /// whether this was the last attempt this save gets.
    fn mark_failed(&mut self, now: Instant) -> bool {
        self.attempts += 1;
        self.next_retry = Some(now + retry_delay(self.attempts));
        self.attempts >= MAX_SYNC_ATTEMPTS
    }
}

/// Backoff before attempt `attempts` (1 = first retry).
fn retry_delay(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(4);
    (RETRY_BASE_DELAY * 2u32.pow(shift)).min(MAX_RETRY_DELAY)
}

/// What the remote file looked like the last time this watcher synced
/// with it, used to detect a concurrent remote change before writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteStamp {
    pub size: Option<u64>,
    pub mtime: Option<u64>,
}

pub fn stamp_from(meta: &Metadata) -> RemoteStamp {
    RemoteStamp {
        size: meta.size,
        mtime: meta.modified,
    }
}

/// Current stamp of `source`, or None when it cannot be read (the file
/// may simply be gone, which the upload then recreates).
fn remote_stamp(session: &Session, source: &str) -> Option<RemoteStamp> {
    let sftp = session.sftp();
    let meta = smol::block_on(async { sftp.metadata(source).await.ok() })?;
    Some(stamp_from(&meta))
}

/// Start watching a freshly downloaded preview file: uploads local
/// saves back to `source` on `session` until the local copy disappears.
/// No-op when this source is already being watched.  `baseline` is the
/// remote stamp taken while downloading, so a remote change made by
/// someone else is detected instead of silently overwritten.
pub fn watch(session: Session, source: String, dest: PathBuf, baseline: Option<RemoteStamp>) {
    if !watched_sources().lock().unwrap().insert(source.clone()) {
        return;
    }
    if ACTIVE_WATCHERS.load(Ordering::Relaxed) >= MAX_WATCHERS {
        watched_sources().lock().unwrap().remove(&source);
        wezterm_toast_notification::persistent_toast_notification(
            "Kaku SFTP",
            &format!("too many open files; edits to {source} will not auto-sync"),
        );
        return;
    }
    let source_for_thread = source.clone();
    let spawned = std::thread::Builder::new()
        .name("sftp-edit-sync".into())
        .spawn(move || {
            ACTIVE_WATCHERS.fetch_add(1, Ordering::Relaxed);
            run(session, source_for_thread.clone(), dest, baseline);
            ACTIVE_WATCHERS.fetch_sub(1, Ordering::Relaxed);
            watched_sources().lock().unwrap().remove(&source_for_thread);
        });
    if spawned.is_err() {
        watched_sources().lock().unwrap().remove(&source);
    }
}

fn run(session: Session, source: String, dest: PathBuf, mut baseline: Option<RemoteStamp>) {
    let mut tracker = SyncTracker::new(stat_of(&dest));
    // Set once a save is refused because the remote copy moved on; the
    // next save then goes through, so a second save is the user's way
    // of saying "overwrite the remote file".
    let mut conflict_warned = false;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        match tracker.observe(stat_of(&dest), now) {
            Action::Idle | Action::Changed => {}
            Action::Stop => return,
            Action::Upload => {
                let stat = stat_of(&dest);
                let remote = remote_stamp(&session, &source);
                if let (Some(known), Some(current)) = (baseline, remote) {
                    if known != current && !conflict_warned {
                        conflict_warned = true;
                        if let Some(stat) = stat.or(tracker.last_seen) {
                            tracker.mark_synced(stat);
                        }
                        log::warn!("sftp edit sync: {source} changed on the server; not saving");
                        wezterm_toast_notification::persistent_toast_notification(
                            "Kaku SFTP",
                            &format!("{source} changed on the server; save again to overwrite it"),
                        );
                        continue;
                    }
                }
                match upload(&session, &source, &dest) {
                    Ok(bytes) => {
                        if let Some(stat) = stat.or(tracker.last_seen) {
                            tracker.mark_synced(stat);
                        }
                        // Our own write becomes the new baseline.
                        baseline = remote_stamp(&session, &source);
                        conflict_warned = false;
                        log::info!("sftp edit sync: uploaded {source} ({bytes} bytes)");
                        wezterm_toast_notification::persistent_toast_notification(
                            "Kaku SFTP",
                            &format!("saved {source} to server ({bytes} bytes)"),
                        );
                    }
                    Err(err) => {
                        let final_attempt = tracker.mark_failed(now);
                        if final_attempt {
                            log::error!(
                                "sftp edit sync: giving up on {source} after {} attempts: {err}",
                                tracker.attempts
                            );
                            wezterm_toast_notification::persistent_toast_notification(
                                "Kaku SFTP",
                                &format!("could not save {source}: {err} - save again to retry"),
                            );
                        } else if tracker.attempts == 1 {
                            // One notification per failing save: the first
                            // failure is the user-visible one, the retries
                            // below are background bookkeeping.
                            log::error!("sftp edit sync: failed to upload {source}: {err}");
                            wezterm_toast_notification::persistent_toast_notification(
                                "Kaku SFTP",
                                &format!("failed to save {source}: {err} (retrying)"),
                            );
                        } else {
                            log::warn!(
                                "sftp edit sync: attempt {} for {source} failed: {err}",
                                tracker.attempts
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Upload the local preview copy over `source`, writing a part file and
/// publishing it with a rename so a failed upload can never leave the
/// remote file truncated.
fn upload(session: &Session, source: &str, dest: &Path) -> Result<u64, String> {
    let sftp = session.sftp();
    let dest_remote = Utf8PathBuf::from(source);
    let part = crate::sftp_transfer::remote_part(&dest_remote);
    smol::block_on(async {
        // A fresh 0666 file would lose the remote mode, and with it the
        // execute bits on scripts and binaries.
        let mode = sftp
            .metadata(&dest_remote)
            .await
            .ok()
            .and_then(|m| m.permissions)
            .map(|p| p.to_unix_mode() & 0o777)
            .filter(|mode| *mode != 0);
        let mut reader = smol::fs::File::open(dest)
            .await
            .map_err(|e| e.to_string())?;
        let mut writer = sftp
            .open_with_mode(
                &part,
                wezterm_ssh::OpenOptions {
                    read: false,
                    write: Some(wezterm_ssh::WriteMode::Write),
                    // The mode is applied by OPEN: no setstat round trip.
                    mode: mode.unwrap_or(0o600) as i32,
                    ty: wezterm_ssh::OpenFileType::File,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut written = 0u64;
        let cancel = AtomicBool::new(false);
        // Same packet size as the transfer engine: one round trip per
        // 256KiB instead of one per 8KiB.
        crate::sftp_transfer::copy_stream(&mut reader, &mut writer, 0, &cancel, &mut |bytes| {
            written = bytes
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
        // No flush: flush is an fsync round trip on this transport and
        // every write is acknowledged before the next one is sent.
        writer.close().await.map_err(|e| e.to_string())?;
        crate::sftp_transfer::publish_remote(&sftp, &part, &dest_remote)
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(written)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        // Instant far-arithmetic is awkward; use a real instant and
        // offsets relative to it in the tests.
        Instant::now() - Duration::from_millis(10_000 - ms)
    }

    fn stat(size: u64) -> Option<Stat> {
        Some(Stat { size, mtime: 100 })
    }

    #[test]
    fn no_change_is_idle() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(10), at(100)), Action::Idle);
    }

    #[test]
    fn change_starts_quiet_period_then_uploads() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        // Quiet period not elapsed yet.
        assert_eq!(
            t.observe(stat(20), at(100) + Duration::from_millis(500)),
            Action::Idle
        );
        // Elapsed: upload.
        assert_eq!(
            t.observe(
                stat(20),
                at(100) + QUIET_PERIOD + Duration::from_millis(600)
            ),
            Action::Upload
        );
    }

    #[test]
    fn continued_editing_extends_the_quiet_period() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        // Another write before the quiet period elapses resets it.
        assert_eq!(
            t.observe(stat(30), at(100) + Duration::from_millis(800)),
            Action::Changed
        );
        assert_eq!(
            t.observe(
                stat(30),
                at(100) + QUIET_PERIOD + Duration::from_millis(200)
            ),
            Action::Idle
        );
        assert_eq!(
            t.observe(
                stat(30),
                at(100) + Duration::from_millis(800) + QUIET_PERIOD + Duration::from_millis(600)
            ),
            Action::Upload
        );
    }

    #[test]
    fn successful_sync_stops_uploading_until_next_change() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        assert_eq!(
            t.observe(stat(20), at(100) + QUIET_PERIOD + Duration::from_secs(1)),
            Action::Upload
        );
        t.mark_synced(stat(20).unwrap());
        assert_eq!(
            t.observe(stat(20), at(100) + QUIET_PERIOD + Duration::from_secs(2)),
            Action::Idle
        );
    }

    #[test]
    fn failed_upload_retries_after_delay() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        let ready = at(100) + QUIET_PERIOD + Duration::from_secs(1);
        assert_eq!(t.observe(stat(20), ready), Action::Upload);
        assert!(!t.mark_failed(ready));
        // Retry delayed.
        assert_eq!(
            t.observe(stat(20), ready + Duration::from_millis(1000)),
            Action::Idle
        );
        assert_eq!(
            t.observe(
                stat(20),
                ready + RETRY_BASE_DELAY + Duration::from_millis(100)
            ),
            Action::Upload
        );
    }

    #[test]
    fn retry_delay_doubles_up_to_the_cap() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(10));
        assert_eq!(retry_delay(3), Duration::from_secs(20));
        assert_eq!(retry_delay(4), Duration::from_secs(40));
        assert_eq!(retry_delay(9), MAX_RETRY_DELAY);
    }

    #[test]
    fn failure_stops_after_the_attempt_budget() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        let mut now = at(100) + QUIET_PERIOD + Duration::from_secs(1);
        for attempt in 1..=MAX_SYNC_ATTEMPTS {
            assert_eq!(
                t.observe(stat(20), now),
                Action::Upload,
                "attempt {attempt}"
            );
            let last = t.mark_failed(now);
            assert_eq!(last, attempt == MAX_SYNC_ATTEMPTS, "attempt {attempt}");
            now += retry_delay(attempt) + Duration::from_millis(1);
        }
        // Budget spent: idle forever, however long the watcher polls.
        assert_eq!(t.observe(stat(20), now), Action::Idle);
        assert_eq!(
            t.observe(stat(20), now + Duration::from_secs(3600)),
            Action::Idle
        );
    }

    #[test]
    fn new_save_restarts_a_spent_budget() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(stat(20), at(100)), Action::Changed);
        for _ in 0..MAX_SYNC_ATTEMPTS {
            t.mark_failed(at(100));
        }
        assert_eq!(t.observe(stat(20), at(100)), Action::Idle);
        // The user edits and saves again: the same stat is never reported
        // twice, so this is a genuinely new change.
        assert_eq!(t.observe(stat(30), at(200)), Action::Changed);
        assert_eq!(
            t.observe(
                stat(30),
                at(200) + QUIET_PERIOD + Duration::from_millis(100)
            ),
            Action::Upload
        );
    }

    #[test]
    fn deleted_file_stops_the_watcher() {
        let mut t = SyncTracker::new(stat(10));
        assert_eq!(t.observe(None, at(100)), Action::Stop);
    }
}
