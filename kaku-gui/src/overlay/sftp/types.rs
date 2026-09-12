//! Shared types for the SFTP dual-pane overlay.

use termwiz::cell::{AttributeChange, CellAttributes};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use wezterm_term::color::ColorPalette;

/// Colors sampled from Kaku's active theme on the GUI thread, following
/// the same derivation the AI chat overlay uses so both overlays feel
/// like one surface.
#[derive(Clone)]
pub struct SftpPalette {
    pub bg: SrgbaTuple,
    pub fg: SrgbaTuple,
    pub accent: SrgbaTuple,
    pub border: SrgbaTuple,
    pub header: SrgbaTuple,
    pub dir: SrgbaTuple,
}

impl SftpPalette {
    fn bg_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.bg)
    }
    fn fg_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.fg)
    }
    fn accent_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.accent)
    }
    fn border_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.border)
    }
    fn header_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.header)
    }
    fn dir_attr(&self) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(self.dir)
    }
    fn make_attrs(&self, fg: ColorAttribute, bg: ColorAttribute) -> CellAttributes {
        let mut a = CellAttributes::default();
        a.set_foreground(fg);
        a.set_background(bg);
        a
    }

    fn make_attrs_bold(&self, fg: ColorAttribute, bg: ColorAttribute) -> CellAttributes {
        let mut a = self.make_attrs(fg, bg);
        a.apply_change(&AttributeChange::Intensity(termwiz::cell::Intensity::Bold));
        a
    }

    pub(crate) fn plain_cell(&self) -> CellAttributes {
        self.make_attrs(self.fg_attr(), self.bg_attr())
    }
    pub(crate) fn dim_cell(&self) -> CellAttributes {
        self.make_attrs(self.border_attr(), self.bg_attr())
    }
    pub(crate) fn border_cell(&self) -> CellAttributes {
        self.make_attrs(self.border_attr(), self.bg_attr())
    }
    pub(crate) fn focused_border_cell(&self) -> CellAttributes {
        self.make_attrs_bold(self.accent_attr(), self.bg_attr())
    }
    pub(crate) fn title_cell(&self) -> CellAttributes {
        self.make_attrs_bold(self.header_attr(), self.bg_attr())
    }
    pub(crate) fn dir_cell(&self) -> CellAttributes {
        self.make_attrs_bold(self.dir_attr(), self.bg_attr())
    }
    pub(crate) fn cursor_cell(&self) -> CellAttributes {
        // Accent background with contrasting foreground, matching the
        // chat overlay's picker cursor treatment.
        self.make_attrs_bold(self.bg_attr(), self.accent_attr())
    }
    pub(crate) fn marked_cell(&self) -> CellAttributes {
        self.make_attrs_bold(self.accent_attr(), self.bg_attr())
    }
    pub(crate) fn error_cell(&self) -> CellAttributes {
        self.make_attrs_bold(self.header_attr(), self.bg_attr())
    }
    pub(crate) fn transfer_cell(&self) -> CellAttributes {
        self.make_attrs(self.accent_attr(), self.bg_attr())
    }
}

/// Build the overlay palette from the terminal's color scheme.
/// Accent = bright cyan (14), border = bright black (8),
/// header = bright yellow (11), directories = bright blue (12).
pub fn sftp_palette(pal: &ColorPalette) -> SftpPalette {
    SftpPalette {
        bg: pal.background,
        fg: pal.foreground,
        accent: pal.colors.0[14],
        border: pal.colors.0[8],
        header: pal.colors.0[11],
        dir: pal.colors.0[12],
    }
}

/// Which half of the dual-pane view has focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelSide {
    Local,
    Remote,
}

impl PanelSide {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::Remote => "Remote",
        }
    }

    pub(crate) fn other(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Local,
        }
    }
}

/// One directory entry, normalized across local fs and sftp.
#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Unix permission bits (0o777), when known.
    pub mode: Option<u32>,
}

/// Sort entries: directories first, then names case-insensitively.
pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Short "drwxr-xr-x"-style permission string for the details column.
pub(crate) fn permission_string(mode: u32, is_dir: bool, is_symlink: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_symlink {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    });
    for triple in [(8, "rwx"), (5, "rwx"), (2, "rwx")] {
        for (i, ch) in triple.1.chars().enumerate() {
            s.push(if mode & (1 << (triple.0 - i)) != 0 {
                ch
            } else {
                '-'
            });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_string_renders_classic_modes() {
        assert_eq!(permission_string(0o755, true, false), "drwxr-xr-x");
        assert_eq!(permission_string(0o644, false, false), "-rw-r--r--");
        assert_eq!(permission_string(0o600, false, false), "-rw-------");
        assert_eq!(permission_string(0o777, false, true), "lrwxrwxrwx");
        assert_eq!(permission_string(0, false, false), "----------");
    }

    #[test]
    fn sort_entries_puts_dirs_first_case_insensitive() {
        let mut entries = vec![
            FileEntry {
                name: "b.txt".into(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                mode: None,
            },
            FileEntry {
                name: "Zebra".into(),
                is_dir: true,
                is_symlink: false,
                size: 0,
                mode: None,
            },
            FileEntry {
                name: "apple".into(),
                is_dir: false,
                is_symlink: false,
                size: 2,
                mode: None,
            },
            FileEntry {
                name: "Alpha".into(),
                is_dir: true,
                is_symlink: false,
                size: 0,
                mode: None,
            },
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Zebra", "apple", "b.txt"]);
    }
}
