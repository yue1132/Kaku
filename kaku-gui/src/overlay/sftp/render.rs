//! Dual-pane rendering for the SFTP overlay, following the AI chat
//! overlay's approach: styled termwiz change sequences inside an atomic
//! `?2026` frame, colors sampled from the terminal theme.

use super::state::{App, InputMode};
use super::types::{permission_string, FileEntry, PanelSide};
use crate::sftp_transfer::{format_bytes, format_speed, TransferState};
use mux::termwiztermtab::TermWizTerminal;
use std::collections::HashMap;
use std::time::Instant;
use termwiz::cell::unicode_column_width;
use termwiz::surface::{Change, CursorVisibility, Position};
use termwiz::terminal::Terminal;

/// Per-transfer last-seen progress used to derive display speed.
pub(crate) struct TransferHistory {
    last: HashMap<u64, (u64, Instant)>,
}

impl TransferHistory {
    pub fn new() -> Self {
        Self {
            last: HashMap::new(),
        }
    }

    /// Returns smoothed bytes/sec for a running transfer.
    fn speed(&mut self, id: u64, bytes: u64) -> Option<f64> {
        let now = Instant::now();
        let speed = match self.last.get(&id) {
            Some((prev_bytes, prev_time)) => {
                let dt = now.duration_since(*prev_time).as_secs_f64();
                if dt >= 0.25 && bytes > *prev_bytes {
                    Some((bytes - prev_bytes) as f64 / dt)
                } else {
                    return None;
                }
            }
            None => return None,
        };
        self.last.insert(id, (bytes, now));
        speed
    }

    pub fn forget(&mut self, id: u64) {
        self.last.remove(&id);
    }
}

pub(crate) fn render(
    term: &mut TermWizTerminal,
    app: &App,
    history: &mut TransferHistory,
) -> termwiz::Result<()> {
    let changes = frame_changes(app, history);
    term.render(&changes)?;
    term.flush()
}

/// Everything one frame draws, including both ends of the synchronized
/// update.  Kept separate from [render] so a test can check that the
/// frame is always closed: an unclosed `?2026` frame leaves the screen
/// frozen on the previous one, which is exactly how `?` looked like a
/// dead key while the key handler was working.
fn frame_changes(app: &App, history: &mut TransferHistory) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    // Atomic frame so partial updates never flicker.
    changes.push(Change::Text("\x1b[?2026h".to_string()));
    changes.push(Change::ClearScreen(termwiz::color::ColorAttribute::Default));

    let cols = app.cols.max(20);
    if app.help {
        render_help(&mut changes, app, cols);

        // The key reference replaces the panels for the rest of the frame.
        changes.push(Change::CursorVisibility(CursorVisibility::Hidden));
        changes.push(Change::Text("\x1b[?2026l".to_string()));
        return changes;
    }

    let left_w = cols / 2;
    let right_w = cols - left_w;
    let visible = app.visible_rows();

    render_panel(&mut changes, app, PanelSide::Local, 0, left_w, visible);
    render_panel(
        &mut changes,
        app,
        PanelSide::Remote,
        left_w,
        right_w,
        visible,
    );

    // ── Transfer row ────────────────────────────────────────────────
    let transfer_row = visible + 2;
    changes.push(Change::CursorPosition {
        x: Position::Absolute(0),
        y: Position::Absolute(transfer_row),
    });
    changes.push(Change::AllAttributes(app.palette.transfer_cell()));
    changes.push(Change::Text(render_transfer_line(app, history, cols)));

    // ── Status / help row ───────────────────────────────────────────
    let status_row = visible + 3;
    changes.push(Change::CursorPosition {
        x: Position::Absolute(0),
        y: Position::Absolute(status_row),
    });
    if let Some(line) = status_line(app) {
        let (attrs, text) = match &line {
            StatusLine::Input(_) => (app.palette.title_cell(), None),
            StatusLine::Connecting(state) => (app.palette.title_cell(), Some(state.to_string())),
            StatusLine::Error(err) => (app.palette.error_cell(), Some(err.to_string())),
            StatusLine::Message(msg) => (app.palette.plain_cell(), Some(msg.to_string())),
            StatusLine::Help(hint) => (app.palette.dim_cell(), Some(hint.clone())),
        };
        changes.push(Change::AllAttributes(attrs));
        match (&line, text) {
            (StatusLine::Input(mode), _) => {
                changes.push(Change::Text(render_input_line(app, mode, cols)));
            }
            (_, Some(text)) => changes.push(Change::Text(truncate_visible(&text, cols))),
            (_, None) => {}
        }
    }

    // ── Bottom focus hint row ───────────────────────────────────────
    let hint_row = visible + 4;
    changes.push(Change::CursorPosition {
        x: Position::Absolute(0),
        y: Position::Absolute(hint_row),
    });
    let focus_label = format!(
        " {} {} ",
        if app.focus == PanelSide::Local {
            "●"
        } else {
            "○"
        },
        PanelSide::Local.label()
    );
    let other_label = format!(
        " {} {} ",
        if app.focus == PanelSide::Remote {
            "●"
        } else {
            "○"
        },
        PanelSide::Remote.label()
    );
    let filler = cols.saturating_sub(focus_label.chars().count() + other_label.chars().count());
    let line = format!("{}{}{}", focus_label, " ".repeat(filler), other_label,);
    if app.session.is_some() {
        changes.push(Change::AllAttributes(app.palette.title_cell()));
    } else {
        changes.push(Change::AllAttributes(app.palette.dim_cell()));
    }
    changes.push(Change::Text(line));

    changes.push(Change::CursorVisibility(CursorVisibility::Hidden));
    changes.push(Change::Text("\x1b[?2026l".to_string()));
    changes
}

fn render_panel(
    changes: &mut Vec<Change>,
    app: &App,
    side: PanelSide,
    x0: usize,
    width: usize,
    visible: usize,
) {
    let panel = app.panel(side);
    let pal = &app.palette;
    let focused = app.focus == side;

    // Top border with the title set in it.
    let title = panel.title();
    let label = if side == PanelSide::Remote {
        if app.session.is_some() && !app.remote_label.is_empty() {
            format!(" {}:{} ", app.remote_label, title)
        } else if app.session.is_some() {
            format!(" remote:{} ", title)
        } else {
            // No session yet: say what the panel is waiting for instead of
            // leaving a blank half with no explanation.
            match &app.connecting {
                Some(state) => format!(" {state} "),
                None => " remote: not connected ".to_string(),
            }
        }
    } else {
        format!(" {} ", title)
    };
    let label = match panel.filter.as_deref() {
        Some(needle) => format!("{label}/{} ", needle),
        None => label,
    };
    let border_cell = if focused {
        pal.focused_border_cell()
    } else {
        pal.border_cell()
    };
    let top = border_row('┌', '┐', '─', &label, width);
    push_line(changes, 0, x0, &top, &border_cell);

    // Entry rows.
    for row in 0..visible {
        let y = row + 1;
        let idx = panel.offset + row;
        let entry = panel.entries.get(idx);
        let is_cursor = idx == panel.cursor && focused;
        let is_marked = panel.marked.contains(&idx);
        let attrs = if is_cursor {
            pal.cursor_cell()
        } else if entry.map(|e| e.is_dir).unwrap_or(false) {
            pal.dir_cell()
        } else if is_marked {
            pal.marked_cell()
        } else {
            pal.plain_cell()
        };
        let body = match entry {
            Some(entry) => format_entry_line(entry, is_marked, width.saturating_sub(2)),
            None => " ".repeat(width.saturating_sub(2)),
        };
        push_bordered_line(changes, y, x0, &body, &attrs, &border_cell);
    }

    // Bottom border.
    let bottom = border_row('└', '┘', '─', "", width);
    push_line(changes, visible + 1, x0, &bottom, &border_cell);
}

fn border_row(left: char, right: char, fill: char, label: &str, width: usize) -> String {
    let label_width = unicode_column_width(label, None);
    let inner = width.saturating_sub(2);
    let shown_label = if label_width > inner { "" } else { label };
    let shown_width = if shown_label.is_empty() {
        0
    } else {
        label_width
    };
    let fills = inner.saturating_sub(shown_width);
    let left_pad = fills / 2;
    let right_pad = fills - left_pad;
    format!(
        "{}{}{}{}{}",
        left,
        fill.to_string().repeat(left_pad),
        shown_label,
        fill.to_string().repeat(right_pad),
        right
    )
}

fn push_line(
    changes: &mut Vec<Change>,
    y: usize,
    x0: usize,
    text: &str,
    cell: &termwiz::cell::CellAttributes,
) {
    changes.push(Change::CursorPosition {
        x: Position::Absolute(x0),
        y: Position::Absolute(y),
    });
    changes.push(Change::AllAttributes(cell.clone()));
    changes.push(Change::Text(text.to_string()));
}

fn push_bordered_line(
    changes: &mut Vec<Change>,
    y: usize,
    x0: usize,
    body: &str,
    body_cell: &termwiz::cell::CellAttributes,
    border_cell: &termwiz::cell::CellAttributes,
) {
    changes.push(Change::CursorPosition {
        x: Position::Absolute(x0),
        y: Position::Absolute(y),
    });
    changes.push(Change::AllAttributes(border_cell.clone()));
    changes.push(Change::Text("│".to_string()));
    changes.push(Change::AllAttributes(body_cell.clone()));
    changes.push(Change::Text(body.to_string()));
    changes.push(Change::AllAttributes(border_cell.clone()));
    changes.push(Change::Text("│".to_string()));
}

/// `[perms] mark name…  size` fitting exactly `width` columns.  The
/// permission column appears only once the pane is wide enough.
fn format_entry_line(entry: &FileEntry, is_marked: bool, width: usize) -> String {
    let mark = if is_marked { "●" } else { " " };
    let size = if entry.is_dir {
        "<DIR>".to_string()
    } else {
        format_bytes(entry.size)
    };
    let size_w = unicode_column_width(&size, None);
    let perms = entry
        .mode
        .map(|m| permission_string(m, entry.is_dir, entry.is_symlink))
        .filter(|_| width >= 26);
    let perms_str = perms.as_deref().unwrap_or("");
    let name_w = width
        .saturating_sub(size_w + 3 + unicode_column_width(perms_str, None))
        .max(1);
    let mut name = truncate_visible(&entry.name, name_w);
    if entry.is_symlink && unicode_column_width(&name, None) < name_w {
        name.push('@');
    }
    let name_pad = name_w.saturating_sub(unicode_column_width(&name, None));
    format!(
        "{}{}{}{}  {}",
        perms_str,
        mark,
        name,
        " ".repeat(name_pad),
        size
    )
}

fn render_transfer_line(app: &App, history: &mut TransferHistory, cols: usize) -> String {
    let statuses = app
        .transfers
        .as_ref()
        .map(|t| t.statuses())
        .unwrap_or_default();
    let active: Vec<_> = statuses
        .iter()
        .filter(|s| !s.state.is_terminal())
        .take(3)
        .collect();
    if active.is_empty() {
        let done: Vec<_> = statuses.iter().filter(|s| s.state.is_terminal()).collect();
        if let Some(last) = done.last() {
            let state = match &last.state {
                TransferState::Done => format!("done · {}", last.dest),
                TransferState::Failed(err) => format!("failed · {err}"),
                TransferState::Cancelled => "cancelled".to_string(),
                _ => String::new(),
            };
            return truncate_visible(
                &format!("{} {} {}", last.direction.glyph(), last.source, state),
                cols,
            );
        }
        return " ".repeat(cols);
    }
    let parts: Vec<String> = active
        .iter()
        .map(|s| match &s.state {
            TransferState::Running { bytes, total } => {
                let pct = if *total > 0 {
                    (*bytes as f64 / *total as f64 * 100.0) as u64
                } else {
                    0
                };
                let speed = history
                    .speed(s.id, *bytes)
                    .map(|v| format!(" {}", format_speed(v)))
                    .unwrap_or_default();
                format!("{} {} {}% {}", s.direction.glyph(), s.source, pct, speed)
            }
            _ => format!("{} {} queued", s.direction.glyph(), s.source),
        })
        .collect();
    truncate_visible(&parts.join("  ·  "), cols)
}

fn render_input_line(app: &App, mode: &InputMode, cols: usize) -> String {
    // One exhaustive match on purpose: this used to be an outer match with a
    // `_ => unreachable!` fallback, so a new input mode panicked the overlay
    // thread at render time (pressing `/` or `z` froze the panel).  Adding a
    // variant now fails to compile instead.
    let text = match mode {
        InputMode::Password { username, prompt } => {
            // Never draw the typed secret; show one bullet per char.
            let masked: String = "●".repeat(app.input_line.chars().count());
            format!("{} for {username}: {masked}_", prompt_title(prompt))
        }
        InputMode::ConfirmHostKey { message } => {
            format!("{} (y/n): ", message.trim_end())
        }
        InputMode::ConfirmOverwrite => {
            let Some(front) = app.pending.first() else {
                return String::new();
            };
            let resume_hint = if front.resumable() { " [r]esume" } else { "" };
            format!(
                "{} exists ({}) - [o]verwrite{} [s]kip [a]ll-ow [x]skip-all ({} left): ",
                front.dest,
                format_bytes(front.size),
                resume_hint,
                app.pending.len(),
            )
        }
        InputMode::ConfirmFolder { side, name } => {
            let short = name
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(name.as_str());
            let target = match side {
                PanelSide::Local => PanelSide::Remote,
                PanelSide::Remote => PanelSide::Local,
            };
            format!(
                "Transfer folder \"{short}\" to {} recursively (overwrite same names)? y/n: ",
                target.label()
            )
        }
        InputMode::Create { directory } => {
            let prompt = if *directory {
                "New directory: "
            } else {
                "New file (end with / for a directory): "
            };
            format!("{prompt}{}_ ", app.input_line)
        }
        InputMode::Rename { original } => format!("Rename {original} to: {}_ ", app.input_line),
        InputMode::ConfirmDelete { names } => {
            let prompt = if names.len() == 1 {
                format!("Delete {}? (y/n): ", names[0])
            } else {
                format!("Delete {} items? (y/n): ", names.len())
            };
            format!("{prompt}{}_ ", app.input_line)
        }
        // Live filter and path jump echo what is typed; the jump-to-char
        // prompt waits for a single letter, so it has no line to show.
        InputMode::Filter => format!("/{}_ ", app.input_line),
        InputMode::JumpTo => format!("jump to path: {}_ ", app.input_line),
        InputMode::JumpToChar => "f jump to a name starting with: _ ".to_string(),
    };
    truncate_visible(&text, cols)
}

/// Capitalize the first letter of an auth prompt for display.
fn prompt_title(prompt: &str) -> String {
    let mut chars = prompt.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// What the status row is showing: the active prompt wins, then what a
/// connection is doing, then the last error, the last message, and
/// finally the key hints.  Exposed for tests because a silent status row
/// is what made a pending connection look like an empty directory.
enum StatusLine<'a> {
    Input(&'a InputMode),
    Connecting(&'a str),
    Error(&'a str),
    Message(&'a str),
    Help(String),
}

fn status_line(app: &App) -> Option<StatusLine<'_>> {
    if let Some(mode) = &app.input_mode {
        return Some(StatusLine::Input(mode));
    }
    if let Some(state) = &app.connecting {
        return Some(StatusLine::Connecting(state));
    }
    if let Some(err) = &app.error {
        return Some(StatusLine::Error(err));
    }
    if let Some(msg) = &app.message {
        return Some(StatusLine::Message(msg));
    }
    Some(StatusLine::Help(help_line(app)))
}

fn help_line(app: &App) -> String {
    if app.session.is_none() {
        return "Not connected · j/k move · Tab switch · q quit".to_string();
    }
    let clipboard = match &app.clipboard {
        Some(clip) if clip.cut => " · CUT ready (p paste)",
        Some(_) => " · copied (p paste)",
        None => "",
    };
    // Marks are a row selection: say how many there are and how to drop
    // them, otherwise they look like they persist by accident.
    let marks = match app.panel(app.focus).marked.len() {
        0 => String::new(),
        n => format!(" · {n} marked (u clear)"),
    };
    format!(
        "j/k move · y/x/p copy-cut-paste · a new · r rename · d delete · Space mark · \
         / filter · f jump · F5 transfer · ? help{marks}{clipboard}"
    )
}

/// F1: the full key reference, replaced by the panels while it is open.
fn render_help(changes: &mut Vec<Change>, app: &App, cols: usize) {
    let pal = &app.palette;
    let title = "Kaku SFTP keys";
    let lines = [
        "j/k ↑/↓ move            Tab   switch panel",
        "h/l ←/→ parent / open   Enter open, Space mark",
        "u clear selection       Ctrl+r invert selection",
        "Ctrl+d/u half page      Ctrl+f/b full page",
        "gg/G top / bottom       o open with default app",
        "y copy  x cut  p paste  P paste (overwrite)",
        "Y / X cancel the yank   D download to ~/Downloads",
        "a new (name/ = folder)  r rename   d delete",
        "/  or F  filter listing  f jump to a name, z jump to a path",
        "c c copy path  c f copy name    Z remote home",
        "H / L    back / forward  . show hidden files",
        "F5 transfer (folders recurse)   F2 rename",
        "F7 new folder                   F8 delete",
        "mouse: click selects, double click opens",
        "q quit   ? or F1 close this help",
    ];
    let width = cols.max(20);
    let top = border_row('┌', '┐', '─', &format!(" {title} "), width);
    push_line(changes, 0, 0, &top, &pal.focused_border_cell());
    for (row, line) in lines.iter().enumerate() {
        let body = format!("{line:<width$}", width = width.saturating_sub(2));
        push_bordered_line(
            changes,
            row + 1,
            0,
            &body[..body.len().min(width.saturating_sub(2))],
            &pal.plain_cell(),
            &pal.focused_border_cell(),
        );
    }
    let bottom = border_row('└', '┘', '─', "", width);
    push_line(
        changes,
        lines.len() + 1,
        0,
        &bottom,
        &pal.focused_border_cell(),
    );
}

/// Truncate to `width` display columns with an ellipsis marker.
fn truncate_visible(text: &str, width: usize) -> String {
    if unicode_column_width(text, None) <= width {
        return text.to_string();
    }
    let mut buf = [0u8; 4];
    let limit = width.saturating_sub(1);
    let mut out = String::new();
    for ch in text.chars() {
        let cw = unicode_column_width(ch.encode_utf8(&mut buf), None);
        if unicode_column_width(&out, None) + cw > limit {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::super::types::SftpPalette;
    use super::*;
    use termwiz::color::SrgbaTuple;

    fn test_app() -> App {
        let palette = SftpPalette {
            bg: SrgbaTuple(0.0, 0.0, 0.0, 1.0),
            fg: SrgbaTuple(1.0, 1.0, 1.0, 1.0),
            accent: SrgbaTuple(0.5, 0.5, 0.5, 1.0),
            border: SrgbaTuple(0.3, 0.3, 0.3, 1.0),
            header: SrgbaTuple(0.4, 0.4, 0.4, 1.0),
            dir: SrgbaTuple(0.2, 0.4, 0.8, 1.0),
        };
        App::new(100, 20, palette, "/tmp".to_string())
    }

    fn frame_text(app: &App) -> String {
        let mut history = TransferHistory::new();
        frame_changes(app, &mut history)
            .iter()
            .filter_map(|change| match change {
                Change::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every input mode must render.  This used to panic for the filter and
    /// jump modes (`unreachable!("handled above")`), which killed the overlay
    /// thread on `/`, `z` and `f`; the match is exhaustive now, and this test
    /// keeps the paths themselves exercised.
    #[test]
    fn every_input_mode_renders() {
        let modes = [
            InputMode::Create { directory: false },
            InputMode::Create { directory: true },
            InputMode::Rename {
                original: "a.txt".into(),
            },
            InputMode::ConfirmDelete {
                names: vec!["a.txt".into()],
            },
            InputMode::ConfirmDelete {
                names: vec!["a.txt".into(), "b.txt".into()],
            },
            InputMode::Password {
                username: "u".into(),
                prompt: "Password".into(),
            },
            InputMode::ConfirmHostKey {
                message: "host".into(),
            },
            InputMode::ConfirmFolder {
                side: PanelSide::Local,
                name: "/tmp/dir".into(),
            },
            InputMode::Filter,
            InputMode::JumpTo,
            InputMode::JumpToChar,
        ];
        for mode in modes {
            let mut app = test_app();
            app.input_line = "typed".into();
            app.input_mode = Some(mode.clone());
            let text = render_input_line(&app, app.input_mode.as_ref().unwrap(), 60);
            assert!(!text.is_empty(), "empty prompt for {:?}", mode);
        }

        // A conflict prompt with an empty queue renders as nothing.
        let mut app = test_app();
        app.input_mode = Some(InputMode::ConfirmOverwrite);
        let text = render_input_line(&app, app.input_mode.as_ref().unwrap(), 60);
        assert!(text.is_empty());
    }

    /// Every frame has to close its synchronized update.  The key reference
    /// used to return early without the closing marker, so pressing `?` set
    /// the state (the key handler really did run) and the screen never
    /// changed: it looked like a dead key.
    #[test]
    fn every_frame_closes_its_synchronized_update() {
        let plain = frame_text(&test_app());
        assert!(plain.starts_with("\x1b[?2026h"), "frame does not open");
        assert!(
            plain.ends_with("\x1b[?2026l"),
            "frame does not close: {:?}",
            plain
        );

        let mut app = test_app();
        app.help = true;
        let help = frame_text(&app);
        assert!(help.contains("Kaku SFTP keys"), "help body missing");
        assert!(
            help.ends_with("\x1b[?2026l"),
            "help frame does not close: {:?}",
            help
        );
    }

    /// A pending connection has to say so: with a silent status row an
    /// empty remote panel looks exactly like an empty directory.
    #[test]
    fn connection_state_is_visible_in_the_status_row() {
        let mut app = test_app();
        assert!(matches!(status_line(&app), Some(StatusLine::Help(_))));

        app.connecting = Some("Connecting to host ...".to_string());
        assert!(matches!(status_line(&app), Some(StatusLine::Connecting(_))));

        // An active prompt outranks the connection text.
        app.input_mode = Some(InputMode::Filter);
        assert!(matches!(status_line(&app), Some(StatusLine::Input(_))));
        app.input_mode = None;

        // A failure outranks a stale message, and both outrank the hints.
        app.connecting = None;
        app.message = Some("older".to_string());
        app.error = Some("boom".to_string());
        assert!(matches!(status_line(&app), Some(StatusLine::Error(_))));
    }

    #[test]
    fn truncate_visible_marks_truncation() {
        assert_eq!(truncate_visible("hello", 10), "hello");
        assert_eq!(truncate_visible("hello world", 8), "hello w…");
    }

    #[test]
    fn entry_line_fits_requested_width() {
        let entry = FileEntry {
            name: "averylongfilename.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 10 * 1024 * 1024,
            mode: None,
        };
        let line = format_entry_line(&entry, false, 30);
        assert!(unicode_column_width(&line, None) <= 30);
        assert!(line.ends_with("10.0 MB"));
        let dir = FileEntry {
            name: "dir".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            mode: None,
        };
        assert!(format_entry_line(&dir, true, 20).starts_with('●'));
        let with_perms = FileEntry {
            mode: Some(0o644),
            ..entry.clone()
        };
        let line = format_entry_line(&with_perms, false, 40);
        assert!(line.starts_with("-rw-r--r--"));
        assert!(line.ends_with("10.0 MB"));
        // Narrow panes drop the permission column instead of the name.
        let narrow = format_entry_line(&with_perms, false, 20);
        assert!(!narrow.starts_with('-'));
    }

    #[test]
    fn border_row_keeps_label_and_width() {
        let row = border_row('┌', '┐', '─', " title ", 20);
        assert_eq!(unicode_column_width(&row, None), 20);
        assert!(row.contains("title"));
    }
}
