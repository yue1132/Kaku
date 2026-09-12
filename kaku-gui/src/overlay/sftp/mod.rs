//! SFTP dual-pane file browser overlay.
//!
//! Activated via Cmd+Shift+R on an SSH pane.  Left pane browses the
//! local filesystem, right pane the remote host over the pane's live
//! ssh session (SSH: domains) or a freshly connected one (ssh CLI
//! panes, agent/key auth).  File operations run on a worker thread;
//! transfers go through the shared [crate::sftp_transfer] engine.

mod input;
pub(crate) mod registry;
mod render;
mod state;

pub(crate) use state::ClipboardWriter;
mod types;

pub(crate) use state::{join_path, scan_local_tree};
pub(crate) use types::sftp_palette;

use input::{handle_input, InputContext};
use mux::pane::PaneId;
use mux::termwiztermtab::TermWizTerminal;
use render::TransferHistory;
use state::{spawn_worker, App, OpRequest, OpResult};
use termwiz::terminal::Terminal;
use types::PanelSide;
use wezterm_ssh::Session;

/// Everything the overlay needs to know about how to reach the remote
/// side, resolved on the GUI thread before the overlay starts.
pub struct SftpOverlayConfig {
    /// Live session from an SSH: domain pane, when available.
    pub session: Option<Session>,
    /// `user@host` to connect to when there is no live session.
    pub connect_target: Option<String>,
    /// Target to redial when the adopted session dies mid-session
    /// (pool and fresh-connect paths; None for domain sessions).
    pub reconnect_target: Option<String>,
    /// Display label for the remote side, e.g. `user@host`.
    pub remote_label: String,
    /// Local directory to start browsing from.
    pub local_path: String,
    /// Remote directory to start browsing from, when already known.
    pub remote_start: Option<String>,
    pub palette: types::SftpPalette,
    /// Writes text to the system clipboard (`c c` / `c f`).
    pub clipboard: Option<state::ClipboardWriter>,
}

/// Show a downloaded file in Finder.
fn reveal_in_finder(path: &std::path::Path) {
    if let Err(err) = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
    {
        log::error!("failed to reveal {}: {err}", path.display());
    }
}

pub fn sftp_overlay(
    pane_id: PaneId,
    mut term: TermWizTerminal,
    config: SftpOverlayConfig,
) -> anyhow::Result<()> {
    term.set_raw_mode()?;
    let size = term.get_screen_size()?;
    let mut app = App::new(size.cols, size.rows, config.palette, config.local_path);
    app.remote_label = config.remote_label.clone();
    app.clipboard_sink = config.clipboard.clone();
    if let Some(start) = &config.remote_start {
        app.panel_mut(PanelSide::Remote).path = start.clone();
    }

    // Decide how the remote side comes online: reuse the live domain
    // session immediately, or start an interactive connect thread.
    let session_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    registry::register(pane_id);
    if let Some(session) = config.session {
        app.session = Some(session.clone());
        *session_slot.lock().unwrap() = Some(session.clone());
        let manager = crate::sftp_transfer::TransferManager::new(session.clone());
        registry::set_transfers(pane_id, manager.clone());
        app.transfers = Some(manager);
        // A domain or pooled session is already authenticated.
        registry::mark_connected(pane_id);
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel::<OpResult>();
    let req_tx = spawn_worker(
        session_slot.clone(),
        OpRequest::Ls {
            side: PanelSide::Local,
            path: app.panel(PanelSide::Local).path.clone(),
        },
        result_tx.clone(),
    )?;

    if let Some(target) = &config.connect_target {
        app.connecting = Some(format!("Connecting to {target} …"));
        state::spawn_connect(target.clone(), session_slot.clone(), result_tx.clone());
    } else if app.session.is_some() {
        // Live domain session: list the remote side right away; the
        // empty path resolves to the remote home.
        req_tx.send(OpRequest::Ls {
            side: PanelSide::Remote,
            path: app.panel(PanelSide::Remote).path.clone(),
        })?;
    }

    // Redial target for dead-session recovery on the pool and
    // fresh-connect paths.
    let pool_target = config.reconnect_target.clone();
    let mut reconnected = false;
    let mut session_died = false;
    let mut needs_queue_start = false;
    // When connecting fresh, the remote listing waits for the session.
    let mut pending_remote_ls = app.session.is_none() && config.connect_target.is_some();
    let mut history = TransferHistory::new();
    let mut needs_redraw = true;

    loop {
        // Drain worker results.
        while let Ok(result) = result_rx.try_recv() {
            match result {
                OpResult::NeedsAuth {
                    username,
                    prompt,
                    reply,
                } => {
                    app.connecting = Some("Authenticating …".to_string());
                    app.auth_reply = Some(state::AuthReply::Password(reply));
                    app.input_mode = Some(state::InputMode::Password { username, prompt });
                    app.input_line.clear();
                }
                OpResult::NeedsHostVerify { message, reply } => {
                    app.connecting = Some("Confirm host key …".to_string());
                    app.auth_reply = Some(state::AuthReply::HostVerify(reply));
                    app.input_mode = Some(state::InputMode::ConfirmHostKey { message });
                    app.input_line.clear();
                }
                OpResult::Connected { session, home } => {
                    app.connecting = None;
                    if app.remote_home.is_none() {
                        app.remote_home = Some(home.clone());
                    }
                    app.session = Some(session.clone());
                    let manager = crate::sftp_transfer::TransferManager::new(session.clone());
                    registry::set_transfers(pane_id, manager.clone());
                    registry::mark_connected(pane_id);
                    registry::update_cwd(pane_id, &home);
                    app.transfers = Some(manager);
                    if app.panel(PanelSide::Remote).path.is_empty() {
                        app.panel_mut(PanelSide::Remote).path = home;
                    }
                    app.focus = PanelSide::Remote;
                    app.set_message("connected");
                    if pending_remote_ls {
                        pending_remote_ls = false;
                        req_tx.send(OpRequest::Ls {
                            side: PanelSide::Remote,
                            path: app.panel(PanelSide::Remote).path.clone(),
                        })?;
                    }
                }
                OpResult::ConnectFailed(err) => {
                    app.connecting = None;
                    app.set_error(err);
                }
                OpResult::Listed {
                    side,
                    path,
                    entries,
                } => {
                    if side == PanelSide::Remote {
                        registry::update_cwd(pane_id, &path);
                        // The first remote listing is the session's
                        // starting directory: that is what `Z` returns to.
                        if app.remote_home.is_none() && entries.is_ok() {
                            app.remote_home = Some(path.clone());
                        }
                    }
                    let panel = app.panel_mut(side);
                    if panel.path == path || panel.path.is_empty() {
                        panel.path = path.clone();
                        match entries {
                            Ok(list) => {
                                panel.set_entries(list);
                                panel.record_visit(&path);
                            }
                            Err(err) => {
                                log::error!("sftp overlay: cannot list {side:?} {path}: {err}");
                                let lower = err.to_lowercase();
                                let dead = lower.contains("dead")
                                    || lower.contains("session")
                                    || lower.contains("channel")
                                    || lower.contains("closed");
                                if dead && side == PanelSide::Remote {
                                    session_died = true;
                                }
                                app.set_error(err);
                            }
                        }
                    }
                }
                OpResult::Opened {
                    source,
                    dest,
                    outcome,
                    stamp,
                } => match outcome {
                    Ok(bytes) => {
                        // Watch the preview copy so editor saves
                        // sync back to the server while open.
                        if let Some(session) = app.session.clone() {
                            crate::sftp_edit_sync::watch(
                                session,
                                source.clone(),
                                dest.clone(),
                                stamp,
                            );
                        }
                        match std::process::Command::new("open").arg(&dest).spawn() {
                            Ok(_) => app.set_message(format!(
                                "opened {} ({}) - saves sync back",
                                dest.display(),
                                crate::sftp_transfer::format_bytes(bytes)
                            )),
                            Err(err) => app.set_error(format!("open failed: {err}")),
                        }
                    }
                    Err(err) => {
                        let lower = err.to_lowercase();
                        if lower.contains("dead")
                            || lower.contains("session")
                            || lower.contains("channel")
                            || lower.contains("closed")
                        {
                            session_died = true;
                        }
                        app.set_error(format!("open failed: {err}"));
                    }
                },
                OpResult::Scanned {
                    side,
                    root,
                    dest,
                    dest_side,
                    cut_source,
                    files,
                } => match files {
                    Ok(list) => {
                        let direction = input::direction_between(side, dest_side);
                        let count = list.len();
                        for f in list {
                            app.pending.push(state::PendingTransfer {
                                direction,
                                source: f.abs,
                                dest: state::join_path(&dest, &f.rel),
                                size: f.size,
                                dest_size: None,
                                cut_source,
                            });
                        }
                        app.set_message(format!("queued {count} files from {root}"));
                        needs_queue_start = true;
                    }
                    Err(err) => app.set_error(format!("scan failed: {err}")),
                },
                OpResult::Mutated { side, outcome } => match outcome {
                    Ok(()) => {
                        app.set_message("done");
                        let path = app.panel(side).path.clone();
                        req_tx.send(OpRequest::Ls { side, path })?;
                    }
                    Err(err) => app.set_error(err),
                },
            }
            needs_redraw = true;
        }

        // Drain transfer events; refresh the receiving side when a
        // transfer lands so new files appear without a manual reload.
        if let Some(transfers) = app.transfers.clone() {
            while let Ok(event) = transfers.events().try_recv() {
                if let crate::sftp_transfer::TransferEvent::Finished(status) = &event {
                    if let crate::sftp_transfer::TransferState::Failed(err) = &status.state {
                        let lower = err.to_lowercase();
                        if lower.contains("dead")
                            || lower.contains("closed channel")
                            || lower.contains("session")
                        {
                            session_died = true;
                        }
                    }
                    history.forget(status.id);
                    // A cut removes its source once the copy landed.
                    if let Some(index) =
                        app.cut_pending.iter().position(|(id, ..)| *id == status.id)
                    {
                        let (_, side, path, is_dir) = app.cut_pending.remove(index);
                        if matches!(status.state, crate::sftp_transfer::TransferState::Done) {
                            req_tx.send(OpRequest::Delete { side, path, is_dir }).ok();
                        }
                    }
                    if let Some((id, path)) = app.pending_reveal.clone() {
                        if id == status.id {
                            app.pending_reveal = None;
                            match &status.state {
                                crate::sftp_transfer::TransferState::Done => {
                                    reveal_in_finder(&path);
                                }
                                crate::sftp_transfer::TransferState::Failed(err) => {
                                    app.set_error(format!("download failed: {err}"));
                                }
                                _ => {}
                            }
                        }
                    }
                    let side = match status.direction {
                        crate::sftp_transfer::Direction::Upload => PanelSide::Remote,
                        crate::sftp_transfer::Direction::Download => PanelSide::Local,
                        // A copy lands on the remote side.
                        crate::sftp_transfer::Direction::Copy => PanelSide::Remote,
                    };
                    if app.session.is_some() {
                        let path = app.panel(side).path.clone();
                        req_tx.send(OpRequest::Ls { side, path }).ok();
                    }
                }
                needs_redraw = true;
            }
        }

        // Any newly queued transfers (from scans or direct F5) that
        // do not conflict start immediately.
        if needs_queue_start {
            needs_queue_start = false;
            input::start_ready_transfers(&mut app);
            if !app.pending.is_empty() && app.input_mode.is_none() {
                app.input_mode = Some(state::InputMode::ConfirmOverwrite);
            }
        }

        // A dead session (listing, open, or transfer failure): redial
        // once transparently instead of leaving the panel broken.
        if session_died && !reconnected {
            if let Some(target) = pool_target.clone() {
                reconnected = true;
                session_died = false;
                if app.session.take().is_some() {
                    crate::sftp_sessions::evict(&target);
                }
                app.transfers = None;
                registry::mark_disconnected(pane_id);
                app.connecting = Some(format!("Reconnecting to {target} ..."));
                state::spawn_connect(target, session_slot.clone(), result_tx.clone());
                pending_remote_ls = true;
            }
        }

        if needs_redraw {
            render::render(&mut term, &app, &mut history)?;
            needs_redraw = false;
        }

        let timeout = if app.connecting.is_some()
            || app
                .transfers
                .as_ref()
                .map(|t| t.statuses().iter().any(|s| !s.state.is_terminal()))
                .unwrap_or(false)
        {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(500)
        };

        match term.poll_input(Some(timeout))? {
            Some(event) => {
                if handle_input(&event, &mut app, InputContext { req_tx: &req_tx }) {
                    // A new message replaces a stale one.
                    needs_redraw = true;
                }
                if app.quit {
                    break;
                }
            }
            None => {
                if app
                    .transfers
                    .as_ref()
                    .map(|t| t.statuses().iter().any(|s| !s.state.is_terminal()))
                    .unwrap_or(false)
                    || app.connecting.is_some()
                {
                    needs_redraw = true;
                }
            }
        }
    }

    // Cancel anything still running and clear the screen before
    // handing the pane back.
    if let Some(transfers) = &app.transfers {
        for status in transfers.statuses() {
            if !status.state.is_terminal() {
                transfers.cancel(status.id);
            }
        }
    }
    term.render(&[
        termwiz::surface::Change::AllAttributes(termwiz::cell::CellAttributes::default()),
        termwiz::surface::Change::ClearScreen(termwiz::color::ColorAttribute::Default),
    ])?;
    Ok(())
}
