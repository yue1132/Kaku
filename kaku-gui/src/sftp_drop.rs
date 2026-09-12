//! Finder drops that land on an ssh pane while the file browser is
//! closed: upload them to that pane's remote directory in the
//! background.

use crate::sftp_transfer::{Direction, TransferEvent, TransferManager, TransferState};
use mux::pane::Pane;
use std::path::PathBuf;
use std::sync::Arc;

/// Upload `paths` to the remote directory of `pane`, when that pane
/// already has a live ssh session.  Returns the remote label for a
/// window message, or None when the caller should keep its default drop
/// behavior (pasting the paths) because there is nothing live to use.
pub(crate) fn upload_to_pane(pane: &Arc<dyn Pane>, paths: &[PathBuf]) -> Option<String> {
    let target = crate::sftp_target::resolve(pane)?;
    let session = target.session?;
    let paths = paths.to_vec();
    let label = target.label;
    let hint = target.cwd;
    let thread_label = label.clone();
    std::thread::Builder::new()
        .name("sftp-drop-upload".into())
        .spawn(move || upload(session, thread_label, hint, paths))
        .ok()?;
    Some(label)
}

fn upload(session: wezterm_ssh::Session, label: String, hint: Option<String>, paths: Vec<PathBuf>) {
    let sftp = session.sftp();
    let cwd = match hint {
        Some(cwd) => cwd,
        // The remote shell never reported a directory: land in the login
        // home, which is where a fresh sftp channel starts.
        None => match smol::block_on(async { sftp.canonicalize(".").await }) {
            Ok(home) => home.to_string(),
            Err(err) => {
                notify(&format!("upload to {label} failed: {err}"));
                return;
            }
        },
    };

    let manager = TransferManager::new(session.clone());
    manager.set_helper_target(label.clone());
    let mut queued = 0usize;
    for path in &paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let dest = crate::overlay::sftp::join_path(&cwd, &name);
        if path.is_dir() {
            // Folders upload recursively, matching F5 in the browser.
            match crate::overlay::sftp::scan_local_tree(&path.to_string_lossy()) {
                Ok(files) => {
                    for file in files {
                        let dest = crate::overlay::sftp::join_path(&dest, &file.rel);
                        manager.upload(file.abs, dest, false);
                        queued += 1;
                    }
                }
                Err(err) => notify(&format!("cannot read {}: {err}", path.display())),
            }
        } else if path.is_file() {
            manager.upload(path.clone(), dest, false);
            queued += 1;
        }
    }
    if queued == 0 {
        notify(&format!("nothing to upload to {label}"));
        return;
    }
    notify(&format!("uploading {queued} file(s) to {label}:{cwd}"));
    retain_background(manager, queued);
}

/// Keep a manager alive until `expected` transfers report a terminal
/// state, reporting each completion through a notification.  Used by
/// transfers started outside the overlay, where no UI is draining events.
fn retain_background(manager: TransferManager, expected: usize) {
    let events = manager.events().clone();
    std::thread::Builder::new()
        .name("sftp-bg-transfer".into())
        .spawn(move || {
            let mut remaining = expected;
            while remaining > 0 {
                match smol::block_on(events.recv()) {
                    Ok(TransferEvent::Updated) => {}
                    Ok(TransferEvent::Finished(status)) => {
                        remaining -= 1;
                        let verb = match status.direction {
                            Direction::Upload => "uploaded",
                            Direction::Download => "downloaded",
                            Direction::Copy => "copied",
                        };
                        let text = match &status.state {
                            TransferState::Done => format!("{verb} {}", status.dest),
                            TransferState::Cancelled => {
                                format!("{verb} {} cancelled", status.dest)
                            }
                            TransferState::Failed(err) => {
                                format!("failed to transfer {}: {err}", status.source)
                            }
                            TransferState::Queued | TransferState::Running { .. } => continue,
                        };
                        wezterm_toast_notification::persistent_toast_notification(
                            "Kaku SFTP",
                            &text,
                        );
                    }
                    Err(_) => break,
                }
            }
            // Dropping the manager here releases the session handle the
            // background transfer was riding on.
            drop(manager);
        })
        .ok();
}

fn notify(message: &str) {
    wezterm_toast_notification::persistent_toast_notification("Kaku SFTP", message);
}
