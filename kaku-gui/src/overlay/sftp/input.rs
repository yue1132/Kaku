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
    let visible = app.visible_rows();
    let side = app.focus;

    if key.modifiers != Modifiers::NONE && key.modifiers != Modifiers::SHIFT {
        // Ctrl+C quits.  The Ctrl movement keys follow yazi: half and
        // full page, plus toggle-all for the selection.  Everything else
        // passes through untouched.
        if key.modifiers == Modifiers::CTRL {
            let page = visible.max(1) as i32;
            let half = (page / 2).max(1);
            let panel = app.panel_mut(side);
            match key.key {
                KeyCode::Char('c') => return Ok(Handled::Quit),
                KeyCode::Char('u') => panel.move_cursor(-half, visible),
                KeyCode::Char('d') => panel.move_cursor(half, visible),
                KeyCode::Char('b') => panel.move_cursor(-page, visible),
                KeyCode::Char('f') => panel.move_cursor(page, visible),
                KeyCode::Char('r') => toggle_all(app, side),
                _ => return Err(false),
            }
            return Ok(Handled::Consumed);
        }
        return Err(false);
    }

    // A pending `g`/`c` prefix is abandoned by any unrelated key, so a
    // half-typed chord cannot swallow the next keystroke.
    let completes_prefix = match key.key {
        KeyCode::Char('g') | KeyCode::Char('c') => true,
        KeyCode::Char('f') if app.pending_c => true,
        _ => false,
    };
    if !completes_prefix {
        app.pending_g = false;
        app.pending_c = false;
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
        // P: paste and overwrite whatever is in the way (yazi's
        // `paste --force`), instead of asking per conflict.
        KeyCode::Char('P') => {
            app.overwrite_all = Some(true);
            paste_clipboard(app, ctx);
            Ok(Handled::Consumed)
        }
        // Y/X: drop the yank state (yazi's unyank).
        KeyCode::Char('Y') | KeyCode::Char('X') => {
            if app.clipboard.take().is_some() {
                app.set_message("yank cancelled");
            } else {
                app.set_message("nothing to cancel");
            }
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
        // z: type a path to jump to; / and F: filter the listing.
        KeyCode::Char('z') => {
            app.input_mode = Some(InputMode::JumpTo);
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        // Z: back to the directory the remote session started in.
        KeyCode::Char('Z') => {
            match app.remote_home.clone() {
                Some(home) => {
                    app.focus = PanelSide::Remote;
                    app.panel_mut(PanelSide::Remote).path = home.clone();
                    ctx.req_tx
                        .send(OpRequest::Ls {
                            side: PanelSide::Remote,
                            path: home,
                        })
                        .ok();
                }
                None => app.set_message("remote home not known yet"),
            }
            Ok(Handled::Consumed)
        }
        // c c / c f: copy the path or the filename to the clipboard,
        // like yazi.  Works on the marks when there are any.
        KeyCode::Char('c') => {
            if app.pending_c {
                copy_paths_to_clipboard(app, false);
                app.pending_c = false;
            } else {
                app.pending_c = true;
                app.set_message("c c copy path · c f copy name");
            }
            Ok(Handled::Consumed)
        }
        // f: jump to the next entry starting with the next key (yazi's
        // jump-to-char).  The plain filter moved to / and F.
        KeyCode::Char('f') if app.pending_c => {
            copy_paths_to_clipboard(app, true);
            app.pending_c = false;
            Ok(Handled::Consumed)
        }
        // Plain `/` filters; a shifted `/` (which is how some layouts and
        // IMEs deliver `?`) opens the key reference below.
        KeyCode::Char('/') if !key.modifiers.contains(Modifiers::SHIFT) => {
            app.input_mode = Some(InputMode::Filter);
            app.input_line = app.panel(side).filter.clone().unwrap_or_default();
            Ok(Handled::Consumed)
        }
        KeyCode::Char('F') => {
            app.input_mode = Some(InputMode::Filter);
            app.input_line = app.panel(side).filter.clone().unwrap_or_default();
            Ok(Handled::Consumed)
        }
        KeyCode::Char('f') => {
            app.input_mode = Some(InputMode::JumpToChar);
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        // F1 and ?: the full key reference (any key closes it).  `?` also
        // arrives as a shifted `/` from some keyboards and IMEs, so both
        // spellings are accepted.
        KeyCode::Function(1) | KeyCode::Char('?') => {
            app.help = true;
            log::info!("sftp overlay: key reference opened");
            Ok(Handled::Consumed)
        }
        KeyCode::Char('/') => {
            app.help = true;
            log::info!("sftp overlay: key reference opened (? as shifted /)");
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
        KeyCode::Function(2) | KeyCode::Char('r') => {
            start_rename(app);
            Ok(Handled::Consumed)
        }
        KeyCode::Function(5) => {
            transfer_marked(app);
            Ok(Handled::Consumed)
        }
        // a: create, yazi-style.  A name ending in `/` is a directory,
        // anything else is an empty file.  F7 keeps making directories.
        KeyCode::Char('a') => {
            app.input_mode = Some(InputMode::Create { directory: false });
            app.input_line.clear();
            Ok(Handled::Consumed)
        }
        KeyCode::Function(7) => {
            app.input_mode = Some(InputMode::Create { directory: true });
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
        // Conflicts and folder confirmations share one queue: answering the
        // last conflict has to hand over to a folder that is still waiting,
        // or it stays in `scan_queue` forever without ever asking.
        start_ready_transfers(app);
    }
    Ok(Handled::Consumed)
}

/// A trailing separator in the typed name means "directory" (yazi's `a`).
fn draws_directory(typed: &str) -> bool {
    typed.ends_with('/')
}

/// Invert the selection over the listing, like yazi's Ctrl+r.
fn toggle_all(app: &mut App, side: PanelSide) {
    let panel = app.panel_mut(side);
    let len = panel.entries.len();
    let previous = std::mem::take(&mut panel.marked);
    panel.marked = (0..len).filter(|i| !previous.contains(i)).collect();
    let marked = app.panel(side).marked.len();
    app.set_message(format!("{marked} selected"));
}

/// `c c` / `c f`: put the marked paths (or the cursor entry) on the
/// clipboard, one per line.
fn copy_paths_to_clipboard(app: &mut App, names_only: bool) {
    let side = app.focus;
    let panel = app.panel(side);
    let dir = panel.path.clone();
    let targets = panel.action_targets();
    if targets.is_empty() {
        app.set_message("nothing to copy");
        return;
    }
    let lines: Vec<String> = targets
        .iter()
        .map(|(_, entry)| {
            if names_only {
                entry.name.clone()
            } else {
                join_path(&dir, &entry.name)
            }
        })
        .collect();
    let text = lines.join("\n");
    match &app.clipboard_sink {
        Some(sink) => {
            sink(text);
            app.set_message(format!("copied {} path(s)", lines.len()));
        }
        None => app.set_message("clipboard unavailable"),
    }
}

/// yazi's `f<char>`: move the cursor to the next entry whose name starts
/// with the typed character, wrapping around the listing.
fn handle_jump_to_char(key: &termwiz::input::KeyEvent, app: &mut App) -> Result<Handled, bool> {
    app.input_mode = None;
    let KeyCode::Char(needle) = key.key else {
        return Ok(Handled::Consumed);
    };
    let needle = needle.to_ascii_lowercase();
    let visible = app.visible_rows();
    let side = app.focus;
    let panel = app.panel(side);
    let len = panel.entries.len();
    if len == 0 {
        return Ok(Handled::Consumed);
    }
    let start = panel.cursor + 1;
    let found = (0..len).map(|step| (start + step) % len).find(|&idx| {
        panel.entries[idx]
            .name
            .chars()
            .next()
            .map(|c| c.to_ascii_lowercase())
            == Some(needle)
    });
    match found {
        Some(idx) => app.panel_mut(side).jump_to(idx, visible),
        None => app.set_message(format!("no entry starts with {needle}")),
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
            let dest = super::state::open_cache_path(&app.remote_label, &source);
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
    // Yanking inside the local panel is a plain local copy: there is no
    // remote side, so the SFTP directions cannot describe it (and handing
    // local paths to copy_remote used to fail with "no such file").
    if clipboard.side == PanelSide::Local && dest_side == PanelSide::Local {
        let mut queued = 0usize;
        for (path, _is_dir, _size) in &clipboard.items {
            let Some(name) = path.rsplit('/').find(|part| !part.is_empty()) else {
                continue;
            };
            ctx.req_tx
                .send(OpRequest::LocalCopy {
                    from: path.clone(),
                    to: join_path(&dest_dir, name),
                    cut: clipboard.cut,
                })
                .ok();
            queued += 1;
        }
        app.set_message(format!(
            "{} {queued} item(s) into {dest_dir}",
            if clipboard.cut { "moving" } else { "copying" }
        ));
        return;
    }
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
        // Local-to-local never reaches the engine: `paste_clipboard` routes
        // it to OpRequest::LocalCopy first.
        (PanelSide::Local, PanelSide::Local) | (PanelSide::Remote, PanelSide::Remote) => {
            Direction::Copy
        }
    }
}

/// Move confirmed-start transfers out of the pending queue; park the
/// conflicting ones behind a confirmation prompt.
pub(crate) fn start_ready_transfers(app: &mut App) {
    match app.transfers.clone() {
        Some(manager) => {
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
        }
        None => {
            // Nothing can start without a connection; drop the queue *and*
            // the answer, so a stale "overwrite all" cannot silently skip
            // the next prompt.  The prompt decision below still runs: a
            // queued folder has to be surfaced (and skippable) instead of
            // sitting in `scan_queue` forever.
            app.pending.clear();
        }
    }
    if app.pending.is_empty() {
        // The answer has been applied to everything it applied to;
        // leaving it set would silently overwrite later conflicts.
        app.overwrite_all = None;
    }
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
        crate::sftp_transfer::Direction::Upload => manager.upload(local, remote, resume, true),
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
        Some(InputMode::JumpToChar) => return handle_jump_to_char(key, app),
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
                Some(InputMode::Create { directory }) => {
                    // A trailing `/` asks for a directory (yazi); F7
                    // always does.  Read it before the line is trimmed.
                    let is_dir = directory || draws_directory(&line);
                    let line = line.trim_end_matches('/').to_string();
                    if line.is_empty() {
                        app.set_error("name is empty");
                    } else {
                        let target = join_path(&path, &line);
                        ctx.req_tx
                            .send(OpRequest::Create {
                                side,
                                path: target,
                                is_dir,
                            })
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

    fn many(app: &mut App, count: usize) {
        let names: Vec<String> = (0..count).map(|i| format!("f{i:02}.txt")).collect();
        app.panel_mut(PanelSide::Remote).set_entries(entries(
            &names.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ));
    }

    /// Ctrl+d/u and Ctrl+b/f move by half and full pages, like yazi.
    /// Every Ctrl chord except Ctrl+C used to be dropped on the floor.
    #[test]
    fn ctrl_keys_move_by_page() {
        let mut app = test_app();
        many(&mut app, 60);
        app.rows = 25; // visible_rows = 20, so half = 10 and page = 20
        let visible = app.visible_rows();
        app.panel_mut(PanelSide::Remote).cursor = 30;

        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let ctrl = |c: char| termwiz::input::KeyEvent {
            key: KeyCode::Char(c),
            modifiers: Modifiers::CTRL,
        };

        handle_key(&ctrl('d'), &mut app, ctx).unwrap();
        assert_eq!(app.panel(PanelSide::Remote).cursor, 30 + visible / 2);
        handle_key(&ctrl('u'), &mut app, ctx).unwrap();
        assert_eq!(app.panel(PanelSide::Remote).cursor, 30);
        handle_key(&ctrl('f'), &mut app, ctx).unwrap();
        assert_eq!(app.panel(PanelSide::Remote).cursor, 30 + visible);
        handle_key(&ctrl('b'), &mut app, ctx).unwrap();
        assert_eq!(app.panel(PanelSide::Remote).cursor, 30);
    }

    /// Ctrl+r inverts the selection (yazi's toggle_all).
    #[test]
    fn ctrl_r_inverts_the_selection() {
        let mut app = test_app();
        many(&mut app, 3);
        app.panel_mut(PanelSide::Remote).marked.insert(0);

        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let ctrl_r = termwiz::input::KeyEvent {
            key: KeyCode::Char('r'),
            modifiers: Modifiers::CTRL,
        };

        handle_key(&ctrl_r, &mut app, ctx).unwrap();
        let marked: Vec<usize> = {
            let mut v: Vec<usize> = app
                .panel(PanelSide::Remote)
                .marked
                .iter()
                .copied()
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(marked, vec![1, 2]);

        handle_key(&ctrl_r, &mut app, ctx).unwrap();
        let marked: Vec<usize> = app
            .panel(PanelSide::Remote)
            .marked
            .iter()
            .copied()
            .collect();
        assert_eq!(marked, vec![0]);
    }

    /// Y and X drop the yank state, like yazi's unyank.
    #[test]
    fn y_and_x_cancel_the_yank() {
        let mut app = test_app();
        app.clipboard = Some(Clipboard {
            side: PanelSide::Remote,
            items: vec![("/remote/a.txt".to_string(), false, 1024)],
            cut: false,
        });
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };

        for key in ['Y', 'X'] {
            app.clipboard = Some(Clipboard {
                side: PanelSide::Remote,
                items: vec![("/remote/a.txt".to_string(), false, 1024)],
                cut: true,
            });
            let press = termwiz::input::KeyEvent {
                key: KeyCode::Char(key),
                modifiers: Modifiers::NONE,
            };
            handle_key(&press, &mut app, ctx).unwrap();
            assert!(app.clipboard.is_none(), "{} left the yank in place", key);
            assert_eq!(app.message.as_deref(), Some("yank cancelled"));
        }
    }

    /// Z jumps back to the directory the remote session started in.
    #[test]
    fn z_returns_to_the_remote_home() {
        let mut app = test_app();
        app.remote_home = Some("/home/admin".to_string());
        app.panel_mut(PanelSide::Remote).path = "/home/admin/deep".to_string();
        app.focus = PanelSide::Local;

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = termwiz::input::KeyEvent {
            key: KeyCode::Char('Z'),
            modifiers: Modifiers::NONE,
        };
        handle_key(&press, &mut app, ctx).unwrap();

        assert_eq!(app.panel(PanelSide::Remote).path, "/home/admin");
        assert_eq!(app.focus, PanelSide::Remote);
        assert!(matches!(
            rx.try_recv(),
            Ok(OpRequest::Ls { side: PanelSide::Remote, path }) if path == "/home/admin"
        ));
    }

    /// c c copies paths and c f copies names, one per line, for the marks.
    #[test]
    fn c_copies_paths_or_names() {
        let mut app = test_app();
        app.panel_mut(PanelSide::Remote)
            .set_entries(entries(&["a.txt", "b.txt", "c.txt"]));
        app.panel_mut(PanelSide::Remote).marked.insert(0);
        app.panel_mut(PanelSide::Remote).marked.insert(2);
        let copied: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = copied.clone();
        app.clipboard_sink = Some(std::sync::Arc::new(move |text: String| {
            sink.lock().unwrap().push(text);
        }));

        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: char| termwiz::input::KeyEvent {
            key: KeyCode::Char(key),
            modifiers: Modifiers::NONE,
        };

        for (key, expected) in [('c', "/remote/a.txt\n/remote/c.txt"), ('f', "a.txt\nc.txt")] {
            handle_key(&press('c'), &mut app, ctx).unwrap();
            handle_key(&press(key), &mut app, ctx).unwrap();
            assert_eq!(copied.lock().unwrap().last().unwrap(), expected);
            assert!(!app.pending_c);
        }
    }

    /// Answering the last conflict must hand over to a folder that is
    /// still waiting for its confirmation, or it sits in `scan_queue`
    /// forever without ever asking.
    #[test]
    fn answering_conflicts_hands_over_to_queued_folders() {
        let mut app = test_app();
        conflicting(&mut app, &["only.bin"]);
        app.scan_queue.push(ScanRequest {
            side: PanelSide::Remote,
            root: "/remote/photos".to_string(),
            dest: "/tmp".to_string(),
            dest_side: PanelSide::Local,
            cut_source: false,
        });

        decide_overwrite(KeyCode::Char('s'), &mut app).unwrap();
        assert!(app.pending.is_empty());
        match &app.input_mode {
            Some(InputMode::ConfirmFolder { name, .. }) => assert_eq!(name, "photos"),
            other => panic!("folder confirmation was not offered: {:?}", other),
        }
    }

    /// Two files with the same name in different remote directories must
    /// not share one local cache file: editing it wrote back to both.
    #[test]
    fn opened_files_get_distinct_cache_paths() {
        let a = crate::overlay::sftp::state::open_cache_path("user@host", "/home/u/a/report.txt");
        let b = crate::overlay::sftp::state::open_cache_path("user@host", "/home/u/b/report.txt");
        let other_host =
            crate::overlay::sftp::state::open_cache_path("other@host", "/home/u/a/report.txt");
        assert_ne!(a, b);
        assert_ne!(a, other_host);
        assert!(a.ends_with("report.txt"));
    }

    /// A half-typed chord does not swallow the next keystroke.
    #[test]
    fn another_key_drops_the_pending_prefix() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: char| termwiz::input::KeyEvent {
            key: KeyCode::Char(key),
            modifiers: Modifiers::NONE,
        };

        handle_key(&press('c'), &mut app, ctx).unwrap();
        assert!(app.pending_c);
        handle_key(&press('j'), &mut app, ctx).unwrap();
        assert!(!app.pending_c);
        assert_eq!(app.panel(PanelSide::Remote).cursor, 0);
    }

    /// F filters the listing, f jumps to the next name starting with a
    /// character (yazi's split of filter and jump-to-char).
    #[test]
    fn f_filters_and_letter_f_jumps() {
        let mut app = test_app();
        app.panel_mut(PanelSide::Remote).set_entries(entries(&[
            "alpha.txt",
            "beta.txt",
            "bravo.txt",
        ]));
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: KeyCode| termwiz::input::KeyEvent {
            key,
            modifiers: Modifiers::NONE,
        };

        handle_key(&press(KeyCode::Char('F')), &mut app, ctx).unwrap();
        assert!(matches!(app.input_mode, Some(InputMode::Filter)));
        app.input_mode = None;

        app.panel_mut(PanelSide::Remote).cursor = 0;
        handle_key(&press(KeyCode::Char('f')), &mut app, ctx).unwrap();
        assert!(matches!(app.input_mode, Some(InputMode::JumpToChar)));
        handle_key(&press(KeyCode::Char('b')), &mut app, ctx).unwrap();
        assert_eq!(app.panel(PanelSide::Remote).cursor, 1);
        assert!(app.input_mode.is_none());
    }

    /// `a` creates a file unless the typed name ends with `/`; F7 always
    /// creates a directory.
    #[test]
    fn create_follows_the_trailing_slash() {
        let mut app = test_app();
        app.focus = PanelSide::Remote;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press = |key: KeyCode| termwiz::input::KeyEvent {
            key,
            modifiers: Modifiers::NONE,
        };
        let type_name = |app: &mut App, name: &str| {
            for ch in name.chars() {
                handle_key(&press(KeyCode::Char(ch)), app, ctx).unwrap();
            }
            handle_key(&press(KeyCode::Enter), app, ctx).unwrap();
        };

        handle_key(&press(KeyCode::Char('a')), &mut app, ctx).unwrap();
        type_name(&mut app, "notes.md");
        assert!(matches!(
            rx.try_recv(),
            Ok(OpRequest::Create { path, is_dir: false, .. }) if path == "/remote/notes.md"
        ));

        handle_key(&press(KeyCode::Char('a')), &mut app, ctx).unwrap();
        type_name(&mut app, "docs/");
        assert!(matches!(
            rx.try_recv(),
            Ok(OpRequest::Create { path, is_dir: true, .. }) if path == "/remote/docs"
        ));

        handle_key(&press(KeyCode::Function(7)), &mut app, ctx).unwrap();
        type_name(&mut app, "plain");
        assert!(matches!(
            rx.try_recv(),
            Ok(OpRequest::Create { path, is_dir: true, .. }) if path == "/remote/plain"
        ));
    }

    /// Some keyboards and IMEs deliver `?` as a shifted `/`, and a
    /// composed `?` arrives as pasted text: all three must reach the key
    /// reference (this is what made "press ? and nothing happens").
    #[test]
    fn question_mark_reaches_the_help_from_every_spelling() {
        for (key, modifiers) in [
            (KeyCode::Char('?'), Modifiers::NONE),
            (KeyCode::Char('?'), Modifiers::SHIFT),
            (KeyCode::Char('/'), Modifiers::SHIFT),
        ] {
            let mut app = test_app();
            let (tx, _rx) = std::sync::mpsc::channel();
            let ctx = InputContext { req_tx: &tx };
            handle_key(&termwiz::input::KeyEvent { key, modifiers }, &mut app, ctx).unwrap();
            assert!(
                app.help,
                "{:?} with {:?} did not open the help",
                key, modifiers
            );
        }

        // A composed `?` arrives as pasted text in the overlay pane.
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        handle_input(&InputEvent::Paste("?".to_string()), &mut app, ctx);
        assert!(app.help, "a composed ? did not open the help");
    }

    /// Plain `/` still filters, and Shift+/ must not be treated as one.
    #[test]
    fn plain_slash_still_filters() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = InputContext { req_tx: &tx };
        let press =
            |key: KeyCode, modifiers: Modifiers| termwiz::input::KeyEvent { key, modifiers };

        handle_key(&press(KeyCode::Char('/'), Modifiers::NONE), &mut app, ctx).unwrap();
        assert!(matches!(app.input_mode, Some(InputMode::Filter)));
        assert!(!app.help);
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
