//! Registry of live SFTP overlays, keyed by the pane that hosts them.
//!
//! Lets GUI-thread entry points — drag-and-drop upload today — reach
//! the overlay's transfer manager and remote cwd without cross-thread
//! access to the overlay's state.

use super::state::{join_path, scan_local_tree};
use crate::sftp_transfer::TransferManager;
use mux::pane::PaneId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) struct OverlayHandle {
    /// Present once a session (domain, pooled, or freshly connected)
    /// is available.
    pub transfers: Mutex<Option<TransferManager>>,
    pub remote_cwd: Mutex<String>,
    pub connected: AtomicBool,
}

fn registry() -> &'static Mutex<HashMap<PaneId, OverlayHandle>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PaneId, OverlayHandle>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register(pane_id: PaneId) {
    registry().lock().unwrap().insert(
        pane_id,
        OverlayHandle {
            transfers: Mutex::new(None),
            remote_cwd: Mutex::new(String::new()),
            connected: AtomicBool::new(false),
        },
    );
}

pub(crate) fn unregister(pane_id: PaneId) {
    registry().lock().unwrap().remove(&pane_id);
}

pub(crate) fn set_transfers(pane_id: PaneId, transfers: TransferManager) {
    if let Some(handle) = registry().lock().unwrap().get(&pane_id) {
        *handle.transfers.lock().unwrap() = Some(transfers);
    }
}

pub(crate) fn mark_connected(pane_id: PaneId) {
    if let Some(handle) = registry().lock().unwrap().get(&pane_id) {
        handle.connected.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn mark_disconnected(pane_id: PaneId) {
    if let Some(handle) = registry().lock().unwrap().get(&pane_id) {
        handle.connected.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn update_cwd(pane_id: PaneId, cwd: &str) {
    if let Some(handle) = registry().lock().unwrap().get(&pane_id) {
        *handle.remote_cwd.lock().unwrap() = cwd.to_string();
    }
}

/// Upload dropped local files into the overlay's remote current
/// directory.  Returns `Some((count, dest_dir))` when the pane has a
/// connected SFTP overlay, `None` to let the caller fall back to the
/// default paste behavior.
pub(crate) fn upload_dropped(pane_id: PaneId, paths: &[PathBuf]) -> Option<(usize, String)> {
    let transfers = {
        let registry = registry();
        let registry = registry.lock().unwrap();
        let handle = registry.get(&pane_id)?;
        if !handle.connected.load(Ordering::Relaxed) {
            return None;
        }
        let cwd = handle.remote_cwd.lock().unwrap().clone();
        let transfers = handle.transfers.lock().unwrap().clone();
        (cwd, transfers)
    };
    let (cwd, transfers) = transfers;
    let transfers = transfers?;
    let mut count = 0usize;
    for path in paths {
        let Some(name) = path.file_name() else {
            continue;
        };
        let dest = join_path(&cwd, &name.to_string_lossy());
        if path.is_dir() {
            // Folders upload recursively, exactly like F5 does.
            match scan_local_tree(&path.to_string_lossy()) {
                Ok(files) => {
                    for file in files {
                        let dest = join_path(&dest, &file.rel);
                        transfers.upload(file.abs, dest, false, false);
                        count += 1;
                    }
                }
                Err(err) => log::error!("SFTP drop: cannot read {}: {err}", path.display()),
            }
            continue;
        }
        if !is_regular_file(path) {
            continue;
        }
        transfers.upload(path.clone(), dest, false, false);
        count += 1;
    }
    if count == 0 {
        None
    } else {
        Some((count, cwd))
    }
}

fn is_regular_file(path: &Path) -> bool {
    path.is_file()
}
