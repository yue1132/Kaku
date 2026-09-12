//! Keyboard handling for the SFTP overlay.  Yazi-style bindings with
//! mc-style function keys for file operations.

use super::state::{join_path, parent_path, App, InputMode, OpRequest, PendingTransfer};
use super::types::PanelSide;
use termwiz::input::{InputEvent, KeyCode, Modifiers};

/// Context the input handler needs to enqueue background work without
/// owning the app borrow.
#[derive(Clone, Copy)]
pub(crate) struct InputContext<'a> {
    pub req_tx: &'a std::sync::mpsc::Sender<OpRequest>,
}

enum Handled {
    Consumed,
    Quit,
}

pub(crate) fn handle_input(event: &InputEvent, app: &mut App, ctx: InputContext<'_>) -> bool {
    // Return value: true when the overlay should redraw.
    match event {
        InputEvent::Key(key) => match handle_key(key, app, ctx) {
            Ok(Handled::Consumed) => true,
            Ok(Handled::Quit) => {
                app.quit = true;
                true
            }
            Err(ignored) => ignored,
        },
        InputEvent::Mouse(mouse) => handle_mouse(mouse, app, ctx),
        InputEvent::Resized { cols, rows } => {
            app.cols = *cols;
            app.rows = *rows;
            true
        }
        // Bytes written to the overlay pane arrive as paste events
        // (ForwardWriter converts pane.writer() output).  In an input
        // line that is text to insert (passwords from a manager, long
        // names for rename/mkdir); in browse mode it is keystrokes to
        // re-parse as real key events.
        InputEvent::Paste(text) if app.input_mode.is_some() => {
            // Overwrite decisions are single-key answers that must
            // work even when an IME turns the keypress into composed
            // text delivered here.
            if matches!(app.input_mode, Some(InputMode::ConfirmOverwrite)) {
                let mut parser = termwiz::input::InputParser::new();
                let keys: Vec<termwiz::input::KeyEvent> = parser
                    .parse_as_vec(text.as_bytes(), false)
                    .into_iter()
                    .filter_map(|ev| match ev {
                        InputEvent::Key(k) => Some(k),
                        _ => None,
                    })
                    .collect();
                let mut redraw = false;
                for key in keys {
                    if let Ok(Handled::Consumed) = decide_overwrite(key.key, app) {
                        redraw = true;
                    }
                }
                redraw
            } else {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                app.input_line.push_str(&clean);
                true
            }
        }
        InputEvent::Paste(text) => {
            let mut parser = termwiz::input::InputParser::new();
            let keys = parser
                .parse_as_vec(text.as_bytes(), false)
                .into_iter()
                .filter_map(|ev| match ev {
                    InputEvent::Key(key) => Some(key),
                    _ => None,
                });
            let mut redraw = false;
            // Not `any`: every keystroke in the paste must be handled.
            for key in keys {
                match handle_key(&key, app, ctx) {
                    Ok(Handled::Consumed) => redraw = true,
                    Ok(Handled::Quit) => {
                        app.quit = true;
                    }
                    Err(ignored) => redraw = redraw || ignored,
                }
            }
            redraw
        }
        _ => false,
    }
}

fn handle_key(
    key: &termwiz::input::KeyEvent,
    app: &mut App,
    ctx: InputContext<'_>,
) -> Result<Handled, bool> {
    if key.modifiers != Modifiers::NONE && key.modifiers != Modifiers::SHIFT {
        // Ctrl+C quits; other chords pass through untouched.
        if key.modifiers == Modifiers::CTRL && key.key == KeyCode::Char('c') {
            return Ok(Handled::Quit);
        }
        return Err(false);
    }

    // Input-line modes capture plain keys first.
    if app.input_mode.is_some() {
        return handle_input_line(key, app, ctx);
    }

    let visible = app.visible_rows();
    let side = app.focus;

    match key.key {
        KeyCode::Char('q') | KeyCode::Escape => Ok(Handled::Quit),
        // j = down/next, k = up/previous (vim/yazi convention).
        KeyCode::UpArrow | KeyCode::Char('k') => {
            app.panel_mut(side).move_cursor(-1, visible);
            app.pending_g = false;
            Ok(Handled::Consumed)
        }
        KeyCode::DownArrow | KeyCode::Char('j') => {
            app.panel_mut(side).move_cursor(1, visible);
            app.pending_g = false;
            Ok(Handled::Consumed)
        }
        KeyCode::LeftArrow | KeyCode::Char('h') => {
            let panel = app.panel_mut(side);
            let parent = parent_path(&panel.path);
            panel.path = parent;
            ctx.req_tx
                .send(OpRequest::Ls {
                    side,
                    path: panel.path.clone(),
                })
                .ok();
            app.pending_g = false;
            Ok(Handled::Consumed)
        }
        KeyCode::RightArrow | KeyCode::Char('l') | KeyCode::Enter => {
            enter_cursor_entry(app, ctx);
            Ok(Handled::Consumed)
        }
        KeyCode::Char('o') => {
            open_cursor_file(app, ctx);
            Ok(Handled::Consumed)
        }
        // D: send the file under the cursor to the download folder and
        // reveal it in Finder when it lands.
        KeyCode::Char('D') => {
            download_cursor_to_downloads(app);
            Ok(Handled::Consumed)
        }
        KeyCode::Char('g') => {
            if app.pending_g {
                app.panel_mut(side).jump_to(0, visible);
                app.pending_g = false;
            } else {
                app.pending_g = true;
            }
            Ok(Handled::Consumed)
        }
        KeyCode::Char('G') => {
            let len = app.panel(side).entries.len();
            app.panel_mut(side).jump_to(len, visible);
            Ok(Handled::Consumed)
        }
        KeyCode::Tab => {
            app.focus = side.other();
            app.pending_g = false;
            Ok(Handled::Consumed)
        }
        KeyCode::Char(' ') => {
            let panel = app.panel_mut(side);
            if panel.entries.is_empty() {
                return Ok(Handled::Consumed);
            }
            if panel.marked.contains(&panel.cursor) {
                panel.marked.remove(&panel.cursor);
            } else {
                panel.marked.insert(panel.cursor);
            }
            panel.move_cursor(1, visible);
            Ok(Handled::Consumed)
        }
        KeyCode::Char('.') => {
            let panel = app.panel_mut(side);
            let path = panel.path.clone();
            panel.toggle_hidden();
            ctx.req_tx.send(OpRequest::Ls { side, path }).ok();
            Ok(Handled::Consumed)
        }
        KeyCode::Function(2) => {
            start_rename(app);
            Ok(Handled::Consumed)
        }
        KeyCode::Function(5) => {
            transfer_marked(app);
            Ok(Handled::Consumed)
        }
        KeyCode::Function(7) => {
            app.input_mode = Some(InputMode::Mkdir);
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        KeyCode::Function(8) | KeyCode::Char('d') => {
            let panel = app.panel(side);
            let targets = panel.action_targets();
            if targets.is_empty() {
                return Ok(Handled::Consumed);
            }
            let names = targets.iter().map(|(_, e)| e.name.clone()).collect();
            app.input_mode = Some(InputMode::ConfirmDelete { names });
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        KeyCode::Home => {
            app.panel_mut(side).jump_to(0, visible);
            Ok(Handled::Consumed)
        }
        KeyCode::End => {
            let len = app.panel(side).entries.len();
            app.panel_mut(side).jump_to(len, visible);
            Ok(Handled::Consumed)
        }
        KeyCode::PageUp => {
            let step = visible as i32;
            app.panel_mut(side).move_cursor(-step, visible);
            Ok(Handled::Consumed)
        }
        KeyCode::PageDown => {
            let step = visible as i32;
            app.panel_mut(side).move_cursor(step, visible);
            Ok(Handled::Consumed)
        }
        _ => Err(false),
    }
}

fn enter_cursor_entry(app: &mut App, ctx: InputContext<'_>) {
    let side = app.focus;
    let Some(entry) = app.panel(side).current_entry().cloned() else {
        return;
    };
    if !entry.is_dir {
        open_cursor_file(app, ctx);
        return;
    }
    let panel = app.panel_mut(side);
    panel.path = join_path(&panel.path, &entry.name);
    ctx.req_tx
        .send(OpRequest::Ls {
            side,
            path: panel.path.clone(),
        })
        .ok();
}

/// Overwrite decision for the front of the pending queue.
/// Keys: o overwrite · r resume (partial only) · s skip ·
/// a overwrite-all · x skip-all · Esc cancel the whole queue.
fn handle_confirm_overwrite(
    key: &termwiz::input::KeyEvent,
    app: &mut App,
) -> Result<Handled, bool> {
    decide_overwrite(key.key.clone(), app)
}

fn decide_overwrite(key: KeyCode, app: &mut App) -> Result<Handled, bool> {
    let Some(front) = app.pending.first().cloned() else {
        app.input_mode = None;
        app.overwrite_all = None;
        return Ok(Handled::Consumed);
    };
    match key {
        KeyCode::Char('o') | KeyCode::Enter => {
            app.pending.remove(0);
            start_front(app, false);
        }
        KeyCode::Char('r') if front.resumable() => {
            app.pending.remove(0);
            start_front(app, true);
        }
        KeyCode::Char('s') => {
            app.pending.remove(0);
        }
        KeyCode::Char('a') => {
            app.overwrite_all = Some(true);
        }
        KeyCode::Char('x') => {
            app.overwrite_all = Some(false);
            app.pending.clear();
        }
        KeyCode::Escape => {
            app.pending.clear();
            app.overwrite_all = None;
        }
        _ => return Ok(Handled::Consumed),
    }
    if app.pending.is_empty() {
        app.input_mode = None;
        app.overwrite_all = None;
    }
    Ok(Handled::Consumed)
}

fn start_front(app: &mut App, resume: bool) {
    let Some(manager) = app.transfers.clone() else {
        return;
    };
    if let Some(item) = app.pending.first() {
        start_transfer(&manager, item, resume);
    }
    app.pending.remove(0);
}

/// Folder recursive-transfer confirmation.  Keys: y/Enter scan and
/// queue · n/Esc skip.
fn handle_confirm_folder(
    key: &termwiz::input::KeyEvent,
    app: &mut App,
    ctx: InputContext<'_>,
) -> Result<Handled, bool> {
    let confirmed = matches!(
        key.key,
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
    );
    let rejected = matches!(
        key.key,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape
    );
    if !confirmed && !rejected {
        return Ok(Handled::Consumed);
    }
    if confirmed {
        if let Some((side, root)) = app.scan_queue.first().cloned() {
            ctx.req_tx.send(OpRequest::ScanTree { side, root }).ok();
        }
    } else {
        app.scan_queue.remove(0);
    }
    if app.scan_queue.is_empty() {
        app.input_mode = None;
    } else if let Some((_, name)) = app.scan_queue.first() {
        let name = name
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        app.input_mode = Some(InputMode::ConfirmFolder {
            side: app.scan_queue[0].0,
            name,
        });
    }
    Ok(Handled::Consumed)
}

/// Open the file under the cursor: local files launch with the
/// default macOS app; remote files download to the preview cache first
/// and open when the transfer lands.
fn open_cursor_file(app: &mut App, ctx: InputContext<'_>) {
    let side = app.focus;
    let Some(entry) = app.panel(side).current_entry().cloned() else {
        return;
    };
    if entry.is_dir {
        return;
    }
    match side {
        PanelSide::Local => {
            let path = join_path(&app.panel(side).path, &entry.name);
            open_local(&path);
            app.set_message(format!("opened {}", entry.name));
        }
        PanelSide::Remote => {
            // Routed through the worker's current session so a stale
            // transfer channel can never break opening; large files
            // simply show "opening ..." until the download lands.
            let source = join_path(&app.panel(side).path, &entry.name);
            let dest = super::state::open_cache_dir().join(&entry.name);
            ctx.req_tx.send(OpRequest::OpenRemote { source, dest }).ok();
            app.set_message(format!("opening {} ...", entry.name));
        }
    }
}

/// Download the file under the cursor to the user's download folder,
/// then reveal it in Finder.  Folders keep using F5, which recurses.
fn download_cursor_to_downloads(app: &mut App) {
    if app.focus != PanelSide::Remote {
        app.set_message("downloads go from the remote panel (Tab switches)");
        return;
    }
    let Some(manager) = app.transfers.clone() else {
        app.set_error("not connected");
        return;
    };
    let Some(entry) = app.panel(PanelSide::Remote).current_entry().cloned() else {
        return;
    };
    if entry.is_dir {
        app.set_message("folders use F5 to download recursively");
        return;
    }
    let dest = match crate::download::resolve_download_path(&entry.name) {
        Ok(dest) => dest,
        Err(err) => {
            app.set_error(format!("no free name in the download folder: {err}"));
            return;
        }
    };
    let source = join_path(&app.panel(PanelSide::Remote).path, &entry.name);
    let id = manager.download(source, dest.clone(), false);
    app.pending_reveal = Some((id, dest.clone()));
    app.set_message(format!("downloading {} to {}", entry.name, dest.display()));
}

fn open_local(path: &str) {
    if let Err(err) = std::process::Command::new("open").arg(path).spawn() {
        app_log_error(path, err);
    }
}

fn app_log_error(path: &str, err: std::io::Error) {
    log::error!("failed to open {path}: {err}");
}

fn start_rename(app: &mut App) {
    let side = app.focus;
    let Some(entry) = app.panel(side).current_entry() else {
        return;
    };
    let original = entry.name.clone();
    app.input_mode = Some(InputMode::Rename {
        original: original.clone(),
    });
    app.input_line = original;
}

/// F5: queue transfers for the selected entries.  Files start (or
/// wait for an overwrite decision); folders ask once and then scan
/// recursively.
fn transfer_marked(app: &mut App) {
    // 大文件并行分片需要辅助连接：告知引擎连接目标。
    if let Some(manager) = &app.transfers {
        manager.set_helper_target(app.remote_label.clone());
    }
    let side = app.focus;
    let other = side.other();
    let dest_dir = app.panel(other).path.clone();
    let targets = app.panel(side).action_targets();
    if targets.is_empty() {
        return;
    }

    for (_, entry) in &targets {
        if entry.is_dir {
            app.scan_queue
                .push((side, join_path(&app.panel(side).path, &entry.name)));
        }
    }

    for (_, entry) in &targets {
        if entry.is_dir {
            continue;
        }
        let source = join_path(&app.panel(side).path, &entry.name);
        let dest = join_path(&dest_dir, &entry.name);
        let dest_size = app
            .panel(other)
            .entries
            .iter()
            .find(|e| e.name == entry.name)
            .map(|e| e.size);
        app.pending.push(PendingTransfer {
            direction: match side {
                PanelSide::Local => crate::sftp_transfer::Direction::Upload,
                PanelSide::Remote => crate::sftp_transfer::Direction::Download,
            },
            source,
            dest,
            size: entry.size,
            dest_size,
        });
    }

    start_ready_transfers(app);
}

/// Move confirmed-start transfers out of the pending queue; park the
/// conflicting ones behind a confirmation prompt.
pub(crate) fn start_ready_transfers(app: &mut App) {
    let Some(manager) = app.transfers.clone() else {
        app.pending.clear();
        return;
    };
    let mut keep: Vec<PendingTransfer> = Vec::new();
    for item in std::mem::take(&mut app.pending) {
        match (item.dest_size.is_some(), app.overwrite_all) {
            (false, _) => start_transfer(&manager, &item, false),
            (_, Some(true)) => start_transfer(&manager, &item, false),
            (_, Some(false)) => {} // skipped
            (true, None) => keep.push(item),
        }
    }
    app.pending = keep;
    if !app.pending.is_empty() && app.input_mode.is_none() {
        app.input_mode = Some(InputMode::ConfirmOverwrite);
    }
}

/// Remote and local path for a queued transfer, in the order the engine
/// expects them.  `source` is the side being read and `dest` the side
/// being written, so which one is remote depends on the direction.
fn transfer_paths(item: &PendingTransfer) -> (String, String) {
    match item.direction {
        crate::sftp_transfer::Direction::Upload => (item.dest.clone(), item.source.clone()),
        crate::sftp_transfer::Direction::Download => (item.source.clone(), item.dest.clone()),
    }
}

fn start_transfer(
    manager: &crate::sftp_transfer::TransferManager,
    item: &PendingTransfer,
    resume: bool,
) {
    let (remote, local) = transfer_paths(item);
    // Upload reads the local side, download reads the remote side.
    match item.direction {
        crate::sftp_transfer::Direction::Upload => manager.upload(local, remote, resume),
        crate::sftp_transfer::Direction::Download => manager.download(remote, local, resume),
    };
}

fn handle_input_line(
    key: &termwiz::input::KeyEvent,
    app: &mut App,
    ctx: InputContext<'_>,
) -> Result<Handled, bool> {
    // Auth prompts answer through their own reply channel instead of
    // enqueueing fs ops.
    match app.input_mode.clone() {
        Some(InputMode::Password { .. }) => return handle_password_line(key, app),
        Some(InputMode::ConfirmHostKey { .. }) => return handle_host_key_line(key, app),
        Some(InputMode::ConfirmOverwrite) => return handle_confirm_overwrite(key, app),
        Some(InputMode::ConfirmFolder { .. }) => return handle_confirm_folder(key, app, ctx),
        _ => {}
    }
    match key.key {
        KeyCode::Enter => {
            let mode = app.input_mode.take();
            let line = std::mem::take(&mut app.input_line);
            let side = app.focus;
            let path = app.panel(side).path.clone();
            match mode {
                Some(InputMode::Mkdir) => {
                    if line.is_empty() {
                        app.set_error("directory name is empty");
                    } else {
                        let target = join_path(&path, &line);
                        ctx.req_tx
                            .send(OpRequest::Mkdir { side, path: target })
                            .ok();
                    }
                }
                Some(InputMode::Rename { original }) => {
                    if line.is_empty() || line == original {
                        app.set_message("rename cancelled");
                    } else {
                        let from = join_path(&path, &original);
                        let to = join_path(&path, &line);
                        ctx.req_tx.send(OpRequest::Rename { side, from, to }).ok();
                    }
                }
                Some(InputMode::ConfirmDelete { names }) => {
                    let confirm = line.trim().eq_ignore_ascii_case("y")
                        || line.trim().eq_ignore_ascii_case("yes");
                    if !confirm {
                        app.set_message("delete cancelled");
                    } else {
                        for name in &names {
                            let Some(entry) = app
                                .panel(side)
                                .entries
                                .iter()
                                .find(|e| &e.name == name)
                                .cloned()
                            else {
                                continue;
                            };
                            let target = join_path(&path, name);
                            ctx.req_tx
                                .send(OpRequest::Delete {
                                    side,
                                    path: target,
                                    is_dir: entry.is_dir,
                                })
                                .ok();
                        }
                    }
                }
                _ => {}
            }
            Ok(Handled::Consumed)
        }
        KeyCode::Escape => {
            app.input_mode = None;
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        KeyCode::Backspace => {
            app.input_line.pop();
            Ok(Handled::Consumed)
        }
        KeyCode::Char(c) => {
            app.input_line.push(c);
            Ok(Handled::Consumed)
        }
        _ => Err(false),
    }
}

/// Masked password entry during the ssh handshake.  Enter sends the
/// typed password to the connect thread, Escape cancels authentication.
fn handle_password_line(key: &termwiz::input::KeyEvent, app: &mut App) -> Result<Handled, bool> {
    match key.key {
        KeyCode::Enter => {
            app.input_mode = None;
            let password = std::mem::take(&mut app.input_line);
            if let Some(super::state::AuthReply::Password(reply)) = app.auth_reply.take() {
                reply.send(Some(password)).ok();
            }
            app.connecting = Some("Authenticating ...".to_string());
            Ok(Handled::Consumed)
        }
        KeyCode::Escape => {
            app.input_mode = None;
            app.input_line.clear();
            if let Some(super::state::AuthReply::Password(reply)) = app.auth_reply.take() {
                reply.send(None).ok();
            }
            app.connecting = Some("Cancelling ...".to_string());
            Ok(Handled::Consumed)
        }
        KeyCode::Backspace => {
            app.input_line.pop();
            Ok(Handled::Consumed)
        }
        KeyCode::Char(c) => {
            app.input_line.push(c);
            Ok(Handled::Consumed)
        }
        _ => Err(false),
    }
}

/// Unknown-host trust decision: y trusts the key, n or Escape rejects.
fn handle_host_key_line(key: &termwiz::input::KeyEvent, app: &mut App) -> Result<Handled, bool> {
    let answer = match key.key {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
        KeyCode::Escape | KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
        _ => None,
    };
    match answer {
        Some(trust) => {
            app.input_mode = None;
            app.input_line.clear();
            if let Some(super::state::AuthReply::HostVerify(reply)) = app.auth_reply.take() {
                reply.send(trust).ok();
            }
            app.connecting = Some("Connecting ...".to_string());
            Ok(Handled::Consumed)
        }
        None => Err(false),
    }
}

/// Mouse support: click focuses a panel and moves the cursor, a
/// second click on the same entry within 400ms opens it (like
/// double-click in a GUI file manager), wheel scrolls.
fn handle_mouse(mouse: &termwiz::input::MouseEvent, app: &mut App, ctx: InputContext<'_>) -> bool {
    use termwiz::input::MouseButtons as B;

    let x = mouse.x as usize;
    let y = mouse.y as usize;

    if mouse.mouse_buttons.contains(B::VERT_WHEEL) {
        let up = mouse.mouse_buttons.contains(B::WHEEL_POSITIVE);
        let side = app.focus;
        let step = 3i32;
        let visible = app.visible_rows();
        app.panel_mut(side)
            .move_cursor(if up { -step } else { step }, visible);
        return true;
    }

    let pressed = mouse.mouse_buttons.intersects(B::LEFT);
    if !pressed || y == 0 || y > app.visible_rows() {
        return false;
    }

    let left_w = app.cols / 2;
    let side = if x < left_w {
        PanelSide::Local
    } else {
        PanelSide::Remote
    };
    app.focus = side;
    let visible = app.visible_rows();
    let idx = {
        let panel = app.panel_mut(side);
        panel.jump_to(panel.offset + (y - 1), visible);
        panel.cursor
    };

    let now = std::time::Instant::now();
    let double_click = matches!(app.last_click.take(), Some((s, i, t))
        if s == side && i == idx && now.duration_since(t) < std::time::Duration::from_millis(400));
    app.last_click = Some((side, idx, now));

    if double_click {
        open_cursor_file(app, ctx);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp_transfer::Direction;

    /// `source`/`dest` name the side being read and the side being
    /// written, so which one is remote flips with the direction.  The
    /// engine always wants (remote, local); reading them off the wrong
    /// field sends an F5 download to a local path over sftp and every
    /// transfer fails with "no such file".
    #[test]
    fn engine_paths_are_remote_then_local() {
        let upload = PendingTransfer {
            direction: Direction::Upload,
            source: "/local/a.bin".into(),
            dest: "/remote/a.bin".into(),
            size: 1,
            dest_size: None,
        };
        assert_eq!(
            transfer_paths(&upload),
            ("/remote/a.bin".to_string(), "/local/a.bin".to_string())
        );

        let download = PendingTransfer {
            direction: Direction::Download,
            source: "/remote/a.bin".into(),
            dest: "/local/a.bin".into(),
            size: 1,
            dest_size: None,
        };
        assert_eq!(
            transfer_paths(&download),
            ("/remote/a.bin".to_string(), "/local/a.bin".to_string())
        );
    }

    #[test]
    fn open_cache_dir_is_usable() {
        let dir = super::super::state::open_cache_dir();
        assert!(dir.ends_with("sftp-open"));
        assert!(dir.exists());
    }
}
