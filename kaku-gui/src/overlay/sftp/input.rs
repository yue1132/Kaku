//! Keyboard handling for the SFTP overlay.  Yazi-style bindings with
//! mc-style function keys for file operations.

use super::state::{
    join_path, parent_path, App, Clipboard, InputMode, OpRequest, PendingTransfer, ScanRequest,
};
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

    // The F1 reference is modal: any key dismisses it.
    if app.help {
        app.help = false;
        return Ok(Handled::Consumed);
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
        // y/x/p: copy, cut and paste, like yazi.
        KeyCode::Char('y') => {
            yank_cursor(app, false);
            Ok(Handled::Consumed)
        }
        KeyCode::Char('x') => {
            yank_cursor(app, true);
            Ok(Handled::Consumed)
        }
        KeyCode::Char('p') => {
            paste_clipboard(app, ctx);
            Ok(Handled::Consumed)
        }
        // H/L: walk the directory history.
        KeyCode::Char('H') => {
            let panel = app.panel_mut(side);
            match panel.back() {
                Some(path) => {
                    panel.path = path.clone();
                    ctx.req_tx.send(OpRequest::Ls { side, path }).ok();
                }
                None => app.set_message("no earlier directory"),
            }
            Ok(Handled::Consumed)
        }
        KeyCode::Char('L') => {
            let panel = app.panel_mut(side);
            match panel.forward() {
                Some(path) => {
                    panel.path = path.clone();
                    ctx.req_tx.send(OpRequest::Ls { side, path }).ok();
                }
                None => app.set_message("no later directory"),
            }
            Ok(Handled::Consumed)
        }
        // z: type a path to jump to; / and f: filter the listing.
        KeyCode::Char('z') => {
            app.input_mode = Some(InputMode::JumpTo);
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        KeyCode::Char('/') | KeyCode::Char('f') => {
            app.input_mode = Some(InputMode::Filter);
            app.input_line = app.panel(side).filter.clone().unwrap_or_default();
            Ok(Handled::Consumed)
        }
        // F1 and ?: the full key reference (any key closes it).
        KeyCode::Function(1) | KeyCode::Char('?') => {
            app.help = true;
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
        // u: drop the whole selection at once (Space toggles one entry).
        KeyCode::Char('u') => {
            let cleared = app.panel_mut(side).marked.len();
            app.panel_mut(side).marked.clear();
            app.set_message(match cleared {
                0 => "nothing selected".to_string(),
                n => format!("selection cleared ({n})"),
            });
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
            start_item(app, &front, false);
        }
        KeyCode::Char('r') if front.resumable() => {
            app.pending.remove(0);
            start_item(app, &front, true);
        }
        KeyCode::Char('s') => {
            app.pending.remove(0);
        }
        KeyCode::Char('a') => {
            app.overwrite_all = Some(true);
            // The front item starts now and the rest of the queue
            // follows without another prompt.
            app.input_mode = None;
            start_ready_transfers(app);
            return Ok(Handled::Consumed);
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

/// Start one queued transfer.  The queue is the caller's business: this
/// used to shift `pending` as well, so an answer consumed two entries
/// and panicked on a one-item queue.
fn start_item(app: &mut App, item: &PendingTransfer, resume: bool) {
    let Some(manager) = app.transfers.clone() else {
        return;
    };
    start_and_note(&manager, app, item, resume);
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
        if let Some(request) = app.scan_queue.first().cloned() {
            ctx.req_tx
                .send(OpRequest::ScanTree {
                    side: request.side,
                    root: request.root,
                    dest: request.dest,
                    dest_side: request.dest_side,
                    cut_source: request.cut_source,
                })
                .ok();
            // The walk is in flight now, so the request has to leave the
            // prompt queue: otherwise the same folder is asked for again
            // once the scan returns and `y` queues it a second time.
            app.scan_queue.remove(0);
        }
    } else {
        app.scan_queue.remove(0);
    }
    // Whatever is queued next owns the prompt: another folder, a
    // conflict, or nothing at all.
    app.input_mode = None;
    start_ready_transfers(app);
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

/// y/x: remember the marked entries (or the cursor) for a later paste.
fn yank_cursor(app: &mut App, cut: bool) {
    let side = app.focus;
    let targets = app.panel(side).action_targets();
    if targets.is_empty() {
        return;
    }
    let items: Vec<(String, bool, u64)> = targets
        .iter()
        .map(|(_, entry)| {
            (
                join_path(&app.panel(side).path, &entry.name),
                entry.is_dir,
                entry.size,
            )
        })
        .collect();
    let count = items.len();
    let verb = if cut { "cut" } else { "copied" };
    app.clipboard = Some(Clipboard { side, items, cut });
    app.set_message(format!("{verb} {count} item(s); p to paste"));
}

/// p: write the clipboard into the focused panel's directory.  Files go
/// through the normal transfer queue, so conflicts still prompt; folders
/// are walked by the worker like F5 does.
fn paste_clipboard(app: &mut App, ctx: InputContext<'_>) {
    let Some(clipboard) = app.clipboard.clone() else {
        app.set_message("nothing to paste (y copies, x cuts)");
        return;
    };
    let dest_side = app.focus;
    let dest_dir = app.panel(dest_side).path.clone();
    let direction = direction_between(clipboard.side, dest_side);
    let mut files = 0usize;
    for (path, is_dir, size) in &clipboard.items {
        let Some(name) = path.rsplit('/').find(|part| !part.is_empty()) else {
            continue;
        };
        if *is_dir {
            app.scan_queue.push(ScanRequest {
                side: clipboard.side,
                root: path.clone(),
                dest: dest_dir.clone(),
                dest_side,
                cut_source: clipboard.cut,
            });
            continue;
        }
        let dest_size = app
            .panel(dest_side)
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.size);
        app.pending.push(PendingTransfer {
            direction,
            source: path.clone(),
            dest: join_path(&dest_dir, name),
            size: *size,
            dest_size,
            cut_source: clipboard.cut,
        });
        files += 1;
    }
    let folders = app.scan_queue.len();
    start_ready_transfers(app);
    // Folder confirmations come from the overlay loop; a paste that only
    // queued files starts right away.
    if folders > 0 {
        let _ = ctx;
        app.set_message(format!(
            "{files} file(s) queued, {folders} folder(s) to confirm"
        ));
    } else if clipboard.cut {
        app.set_message(format!("cut {files} item(s) into {dest_dir}"));
    } else {
        app.set_message(format!("copied {files} item(s) into {dest_dir}"));
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
            app.scan_queue.push(ScanRequest {
                side,
                root: join_path(&app.panel(side).path, &entry.name),
                dest: dest_dir.clone(),
                dest_side: other,
                cut_source: false,
            });
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
            direction: direction_between(side, other),
            source,
            dest,
            size: entry.size,
            dest_size,
            cut_source: false,
        });
    }

    // The selection is consumed by the action it was made for: the
    // entries are queued now, so leaving them marked would ask the user
    // to press Space over every file a second time.
    let queues = app.panel(side).marked.len();
    app.panel_mut(side).marked.clear();
    if queues > 1 {
        app.set_message(format!("{queues} files queued"));
    }

    start_ready_transfers(app);
}

/// Which way a transfer goes between two panels: reading local and
/// writing remote is an upload, the reverse is a download, and the same
/// side on both ends is a remote or local copy.
pub(crate) fn direction_between(from: PanelSide, to: PanelSide) -> crate::sftp_transfer::Direction {
    use crate::sftp_transfer::Direction;
    match (from, to) {
        (PanelSide::Local, PanelSide::Remote) => Direction::Upload,
        (PanelSide::Remote, PanelSide::Local) => Direction::Download,
        _ => Direction::Copy,
    }
}

/// Move confirmed-start transfers out of the pending queue; park the
/// conflicting ones behind a confirmation prompt.
pub(crate) fn start_ready_transfers(app: &mut App) {
    let Some(manager) = app.transfers.clone() else {
        // Without a connection nothing can start; drop the queue *and* the
        // answer, so a stale "overwrite all" cannot silently skip the
        // next prompt.
        app.pending.clear();
        app.overwrite_all = None;
        return;
    };
    let mut keep: Vec<PendingTransfer> = Vec::new();
    for item in std::mem::take(&mut app.pending) {
        match (item.dest_size.is_some(), app.overwrite_all) {
            (false, _) => start_and_note(&manager, app, &item, false),
            (_, Some(true)) => start_and_note(&manager, app, &item, false),
            (_, Some(false)) => {} // skipped
            (true, None) => keep.push(item),
        }
    }
    app.pending = keep;
    if app.input_mode.is_some() {
        return;
    }
    // Conflicts are answered first; then folders waiting for their
    // recursive-transfer confirmation.  Without this, queueing a folder
    // left it stuck: the first prompt was never shown.
    if !app.pending.is_empty() {
        app.input_mode = Some(InputMode::ConfirmOverwrite);
    } else if let Some(request) = app.scan_queue.first() {
        app.input_mode = Some(folder_prompt(request));
    }
}

/// Confirmation shown before a folder is walked recursively.
fn folder_prompt(request: &ScanRequest) -> InputMode {
    let name = request
        .root
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_string();
    InputMode::ConfirmFolder {
        side: request.side,
        name,
    }
}

/// Remote and local path for a queued transfer, in the order the engine
/// expects them.  `source` is the side being read and `dest` the side
/// being written, so which one is remote depends on the direction.
fn transfer_paths(item: &PendingTransfer) -> (String, String) {
    match item.direction {
        // The engine wants (remote, local); a copy has both on the remote.
        crate::sftp_transfer::Direction::Upload => (item.dest.clone(), item.source.clone()),
        crate::sftp_transfer::Direction::Download => (item.source.clone(), item.dest.clone()),
        crate::sftp_transfer::Direction::Copy => (item.source.clone(), item.dest.clone()),
    }
}

/// Start one transfer and remember a cut's source, so a successful copy
/// can remove it.
fn start_and_note(
    manager: &crate::sftp_transfer::TransferManager,
    app: &mut App,
    item: &PendingTransfer,
    resume: bool,
) {
    let id = start_transfer(manager, item, resume);
    if !item.cut_source {
        return;
    }
    // The cut removes what was read: the local file for an upload, the
    // remote file otherwise.
    let side = match item.direction {
        crate::sftp_transfer::Direction::Upload => PanelSide::Local,
        crate::sftp_transfer::Direction::Download | crate::sftp_transfer::Direction::Copy => {
            PanelSide::Remote
        }
    };
    app.cut_pending.push((id, side, item.source.clone(), false));
}

fn start_transfer(
    manager: &crate::sftp_transfer::TransferManager,
    item: &PendingTransfer,
    resume: bool,
) -> crate::sftp_transfer::TransferId {
    let (remote, local) = transfer_paths(item);
    // Upload reads the local side, download reads the remote side.
    match item.direction {
        crate::sftp_transfer::Direction::Upload => manager.upload(local, remote, resume),
        crate::sftp_transfer::Direction::Download => manager.download(remote, local, resume),
        crate::sftp_transfer::Direction::Copy => manager.copy_remote(remote, local),
    }
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
        Some(InputMode::JumpTo) => return handle_jump_line(key, app, ctx),
        Some(InputMode::Filter) => {
            return handle_filter_line(key, app);
        }
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

/// `/` or `f`: narrow the listing as the filter is typed.
fn handle_filter_line(key: &termwiz::input::KeyEvent, app: &mut App) -> Result<Handled, bool> {
    let side = app.focus;
    match key.key {
        KeyCode::Enter => {
            app.input_mode = None;
            let kept = app.panel(side).entries.len();
            app.set_message(format!("{kept} matching entr(y/ies)"));
        }
        KeyCode::Escape => {
            app.input_mode = None;
            app.input_line.clear();
            app.panel_mut(side).set_filter(None);
        }
        KeyCode::Backspace => {
            app.input_line.pop();
            let needle = app.input_line.clone();
            app.panel_mut(side).set_filter(Some(needle));
        }
        KeyCode::Char(c) => {
            app.input_line.push(c);
            let needle = app.input_line.clone();
            app.panel_mut(side).set_filter(Some(needle));
        }
        _ => return Err(false),
    }
    Ok(Handled::Consumed)
}

/// `z`: type a directory to jump to, absolute or relative to the pane.
fn handle_jump_line(
    key: &termwiz::input::KeyEvent,
    app: &mut App,
    ctx: InputContext<'_>,
) -> Result<Handled, bool> {
    let side = app.focus;
    match key.key {
        KeyCode::Enter => {
            app.input_mode = None;
            let line = std::mem::take(&mut app.input_line);
            let base = app.panel(side).path.clone();
            match jump_target(&base, &line) {
                Some(path) => {
                    app.panel_mut(side).path = path.clone();
                    ctx.req_tx.send(OpRequest::Ls { side, path }).ok();
                }
                None => app.set_error("no directory given"),
            }
        }
        KeyCode::Escape => {
            app.input_mode = None;
            app.input_line.clear();
        }
        KeyCode::Backspace => {
            app.input_line.pop();
        }
        KeyCode::Char(c) => {
            app.input_line.push(c);
        }
        _ => return Err(false),
    }
    Ok(Handled::Consumed)
}

/// Directory a typed jump lands in, or None for empty input.  Supports
/// absolute paths, `~`, `.`/`..` and plain relative names.
pub(crate) fn jump_target(cwd: &str, typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    if typed == "~" || typed.starts_with("~/") {
        let home = dirs_next::home_dir()?;
        let home = home.to_string_lossy().to_string();
        return Some(match typed.strip_prefix('~') {
            Some("") => home,
            Some(rest) => format!("{home}{rest}"),
            None => home,
        });
    }
    if typed.starts_with('/') {
        return Some(typed.to_string());
    }
    if typed == "." {
        return Some(cwd.to_string());
    }
    if typed == ".." {
        return Some(parent_path(cwd));
    }
    Some(join_path(cwd, typed))
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
    // A physical click arrives as a press and then a release, and a drag
    // keeps reporting the held button: only the false-to-true edge counts
    // as a click, so dragging never opens the entry under the cursor.
    let clicked = pressed && !app.mouse_left_down;
    app.mouse_left_down = pressed;
    if !clicked || y == 0 || y > app.visible_rows() {
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
    use super::super::types::FileEntry;
    use super::*;
    use crate::sftp_transfer::Direction;
    use termwiz::color::SrgbaTuple;
    use termwiz::input::{MouseButtons, MouseEvent};

    fn test_app() -> App {
        let palette = super::super::types::SftpPalette {
            bg: SrgbaTuple(0.0, 0.0, 0.0, 1.0),
            fg: SrgbaTuple(1.0, 1.0, 1.0, 1.0),
            accent: SrgbaTuple(0.5, 0.5, 0.5, 1.0),
            border: SrgbaTuple(0.3, 0.3, 0.3, 1.0),
            header: SrgbaTuple(0.4, 0.4, 0.4, 1.0),
            dir: SrgbaTuple(0.2, 0.4, 0.8, 1.0),
        };
        let mut app = App::new(100, 20, palette, "/tmp".to_string());
        app.focus = PanelSide::Remote;
        let panel = app.panel_mut(PanelSide::Remote);
        panel.path = "/remote".to_string();
        panel.set_entries(vec![FileEntry {
            name: "a.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 1,
            mode: Some(0o644),
        }]);
        app
    }

    fn row(x: u16, buttons: MouseButtons) -> MouseEvent {
        MouseEvent {
            x,
            y: 1,
            mouse_buttons: buttons,
            modifiers: Modifiers::NONE,
        }
    }

    /// One physical click reaches the overlay as a press followed by a
    /// release with no button held (see `mux::termwiztermtab`), so it must
    /// only select; opening is reserved for a second press.
    #[test]
    fn one_click_selects_and_a_second_press_opens() {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let mut app = test_app();

        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::NONE), &mut app, ctx);
        assert!(
            rx.try_recv().is_err(),
            "a single click must not open the entry"
        );

        // A drag keeps reporting the held button; it must not open either.
        // Clear the double-click window so each phase stands alone.
        app.last_click = None;
        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::NONE), &mut app, ctx);
        assert!(rx.try_recv().is_err(), "dragging must not open the entry");

        app.last_click = None;
        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::NONE), &mut app, ctx);
        handle_mouse(&row(60, MouseButtons::LEFT), &mut app, ctx);
        assert!(
            matches!(rx.try_recv(), Ok(OpRequest::OpenRemote { .. })),
            "a double click must open the entry"
        );
    }

    /// Which way a paste goes decides which engine call runs; getting it
    /// wrong is how the earlier F5 download bug happened.
    /// Queue transfers that all conflict with an existing destination.
    fn conflicting(app: &mut App, names: &[&str]) {
        for name in names {
            app.pending.push(PendingTransfer {
                direction: Direction::Download,
                source: format!("/remote/{name}"),
                dest: format!("/tmp/{name}"),
                size: 4096,
                dest_size: Some(2048),
                cut_source: false,
            });
        }
        app.input_mode = Some(InputMode::ConfirmOverwrite);
    }

    /// Confirming a folder walk takes the request off the prompt queue.
    /// Otherwise the scan comes back while the old prompt is still up, the
    /// same folder is asked for again, and a second `y` queues it twice.
    #[test]
    fn folder_confirmation_clears_the_prompt() {
        let mut app = test_app();
        let request = |root: &str| ScanRequest {
            side: PanelSide::Local,
            root: root.to_string(),
            dest: "/remote".to_string(),
            dest_side: PanelSide::Remote,
            cut_source: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: KeyCode| termwiz::input::KeyEvent {
            key,
            modifiers: Modifiers::NONE,
        };

        app.scan_queue.push(request("/tmp/photos"));
        app.input_mode = Some(folder_prompt(app.scan_queue.first().unwrap()));
        handle_confirm_folder(&press(KeyCode::Char('y')), &mut app, ctx).unwrap();
        assert!(app.scan_queue.is_empty(), "confirmed folder stayed queued");
        assert!(app.input_mode.is_none(), "prompt survived the answer");
        assert!(matches!(rx.try_recv(), Ok(OpRequest::ScanTree { .. })));

        // Rejecting the next folder also leaves nothing behind.
        app.scan_queue.push(request("/tmp/other"));
        app.input_mode = Some(folder_prompt(app.scan_queue.first().unwrap()));
        handle_confirm_folder(&press(KeyCode::Char('n')), &mut app, ctx).unwrap();
        assert!(app.scan_queue.is_empty());
        assert!(app.input_mode.is_none());
        assert!(
            rx.try_recv().is_err(),
            "a rejected folder was still scanned"
        );
    }

    fn entries(names: &[&str]) -> Vec<FileEntry> {
        names
            .iter()
            .map(|name| FileEntry {
                name: (*name).to_string(),
                is_dir: false,
                is_symlink: false,
                size: 8,
                mode: Some(0o644),
            })
            .collect()
    }

    /// `?` opens the key reference, and any key closes it again.
    #[test]
    fn question_mark_opens_the_help() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: KeyCode| termwiz::input::KeyEvent {
            key,
            modifiers: Modifiers::NONE,
        };

        handle_key(&press(KeyCode::Char('?')), &mut app, ctx).unwrap();
        assert!(app.help, "? did not open the key reference");

        handle_key(&press(KeyCode::Char('j')), &mut app, ctx).unwrap();
        assert!(!app.help, "a key press did not close the key reference");
    }

    /// `u` drops the whole selection; Space is the per-entry toggle.
    #[test]
    fn u_clears_the_selection() {
        let mut app = test_app();
        let panel = app.panel_mut(PanelSide::Remote);
        panel.set_entries(entries(&["a.txt", "b.txt", "c.txt"]));
        panel.marked.insert(0);
        panel.marked.insert(2);

        let (tx, _rx) = std::sync::mpsc::channel();
        let key = termwiz::input::KeyEvent {
            key: KeyCode::Char('u'),
            modifiers: Modifiers::NONE,
        };
        handle_key(&key, &mut app, InputContext { req_tx: &tx }).unwrap();

        assert!(app.panel(PanelSide::Remote).marked.is_empty());
        assert_eq!(app.message.as_deref(), Some("selection cleared (2)"));
    }

    /// The selection belongs to the action it was made for: once F5 has
    /// queued the entries, they must not stay marked, or the file that was
    /// just uploaded keeps looking as if it were still selected.
    #[test]
    fn transfer_consumes_the_selection() {
        let mut app = test_app();
        let panel = app.panel_mut(PanelSide::Remote);
        panel.set_entries(entries(&["a.txt", "b.txt", "c.txt"]));
        panel.marked.insert(0);
        panel.marked.insert(1);

        transfer_marked(&mut app);

        assert!(app.panel(PanelSide::Remote).marked.is_empty());
        assert_eq!(app.message.as_deref(), Some("2 files queued"));
    }

    /// One answer consumes exactly one queued transfer.  The queue used to
    /// be consumed twice per answer: it started the *second* entry by
    /// mistake and panicked with "removal index (is 0) should be < len (is
    /// 0)" on a one-item queue, which killed the overlay thread behind a
    /// frozen prompt that ignored every later key.
    #[test]
    fn one_answer_consumes_one_queued_transfer() {
        let mut app = test_app();
        conflicting(&mut app, &["a.bin", "b.bin"]);

        decide_overwrite(KeyCode::Char('o'), &mut app).unwrap();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].dest, "/tmp/b.bin");
        assert!(matches!(app.input_mode, Some(InputMode::ConfirmOverwrite)));

        decide_overwrite(KeyCode::Char('s'), &mut app).unwrap();
        assert!(app.pending.is_empty());
        assert!(app.input_mode.is_none());
    }

    /// The one-item queue is the exact shape that panicked.
    #[test]
    fn a_single_conflict_answers_without_panicking() {
        let mut app = test_app();
        conflicting(&mut app, &["only.bin"]);

        decide_overwrite(KeyCode::Char('o'), &mut app).unwrap();
        assert!(app.pending.is_empty());
        assert!(app.input_mode.is_none());
    }

    /// "overwrite all" has to clear the prompt itself: nothing is left to
    /// conflict with, so the queue would stay parked behind an empty
    /// prompt.
    #[test]
    fn overwrite_all_clears_the_prompt() {
        let mut app = test_app();
        conflicting(&mut app, &["a.bin", "b.bin"]);

        decide_overwrite(KeyCode::Char('a'), &mut app).unwrap();
        assert!(app.pending.is_empty());
        assert!(app.input_mode.is_none());
        assert!(app.overwrite_all.is_none());
    }

    /// "skip all" drops the whole queue and the prompt with it.
    #[test]
    fn skip_all_clears_the_prompt() {
        let mut app = test_app();
        conflicting(&mut app, &["a.bin", "b.bin"]);

        decide_overwrite(KeyCode::Char('x'), &mut app).unwrap();
        assert!(app.pending.is_empty());
        assert!(app.input_mode.is_none());
    }

    #[test]
    fn paste_direction_follows_the_two_sides() {
        use crate::sftp_transfer::Direction;
        assert_eq!(
            direction_between(PanelSide::Local, PanelSide::Remote),
            Direction::Upload
        );
        assert_eq!(
            direction_between(PanelSide::Remote, PanelSide::Local),
            Direction::Download
        );
        assert_eq!(
            direction_between(PanelSide::Remote, PanelSide::Remote),
            Direction::Copy
        );
        assert_eq!(
            direction_between(PanelSide::Local, PanelSide::Local),
            Direction::Copy
        );
    }

    #[test]
    fn jump_target_resolves_paths() {
        assert_eq!(jump_target("/root/app", "/etc").as_deref(), Some("/etc"));
        assert_eq!(
            jump_target("/root/app", "sub/dir").as_deref(),
            Some("/root/app/sub/dir")
        );
        assert_eq!(jump_target("/root/app", "..").as_deref(), Some("/root"));
        assert_eq!(jump_target("/root/app", ".").as_deref(), Some("/root/app"));
        assert_eq!(jump_target("/root/app", "   "), None);
        let home = jump_target("/root/app", "~").unwrap_or_default();
        assert!(!home.is_empty() && home != "/root/app");
    }

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
            cut_source: false,
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
            cut_source: false,
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
