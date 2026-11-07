//! The screen-state model: what a mirrored terminal looks like in memory and
//! on the wire.
//!
//! Two forms of a row exist on purpose:
//!
//! - [`Row`] is the decoded form, one [`Cell`] per column. The producer diffs
//!   these and the mirrored grid renders from them, so column lookup is O(1).
//! - [`WireRow`] is the encoded form: runs of same-styled text with trailing
//!   blanks trimmed. A typical 200-column row is a handful of runs, which is
//!   what keeps a full-screen diff small.
//!
//! Colours stay as alacritty's `Color` in memory so the grid element's colour
//! mapping is the same code for a live `Term` and a mirror. On the wire they
//! are a single `u32` ([`color_code`]) so the format does not depend on how
//! alacritty happens to serialise its enums.

use std::hash::{Hash, Hasher};

use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use serde::{Deserialize, Serialize};

use crate::errors::{AlleleError, Result};

// ── Colours ────────────────────────────────────────────────────────

/// Every `NamedColor`, in wire order. The index is the wire code (offset by
/// [`NAMED_BASE`]). Append only: reordering changes the meaning of old frames.
const NAMED: [NamedColor; 29] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
    NamedColor::Foreground,
    NamedColor::Background,
    NamedColor::Cursor,
    NamedColor::DimBlack,
    NamedColor::DimRed,
    NamedColor::DimGreen,
    NamedColor::DimYellow,
    NamedColor::DimBlue,
    NamedColor::DimMagenta,
    NamedColor::DimCyan,
    NamedColor::DimWhite,
    NamedColor::BrightForeground,
    NamedColor::DimForeground,
];

const NAMED_BASE: u32 = 0x100;
const RGB_TAG: u32 = 0x0100_0000;

/// Encode a colour as one integer: `0..=255` indexed, `0x100 + n` named,
/// `0x0100_0000 | rgb` true colour.
pub fn color_code(color: AnsiColor) -> u32 {
    match color {
        AnsiColor::Indexed(i) => u32::from(i),
        AnsiColor::Named(named) => {
            // NAMED lists every variant, so the fallback is unreachable today;
            // it maps a variant added by a future vte to the foreground.
            let idx = NAMED.iter().position(|n| *n == named).unwrap_or(16);
            NAMED_BASE + idx as u32
        }
        AnsiColor::Spec(Rgb { r, g, b }) => {
            RGB_TAG | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
    }
}

/// Decode [`color_code`]. `None` for a code no encoder produces.
pub fn color_from_code(code: u32) -> Option<AnsiColor> {
    if code < NAMED_BASE {
        return Some(AnsiColor::Indexed(code as u8));
    }
    if code & 0xFF00_0000 == RGB_TAG {
        return Some(AnsiColor::Spec(Rgb {
            r: (code >> 16) as u8,
            g: (code >> 8) as u8,
            b: code as u8,
        }));
    }
    NAMED
        .get((code - NAMED_BASE) as usize)
        .map(|n| AnsiColor::Named(*n))
}

fn decode_color(code: u32) -> Result<AnsiColor> {
    color_from_code(code).ok_or_else(|| AlleleError::Mirror(format!("bad colour code {code:#x}")))
}

// ── Cells ──────────────────────────────────────────────────────────

/// Colours and attribute flags of a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: AnsiColor,
    pub bg: AnsiColor,
    pub flags: Flags,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: AnsiColor::Named(NamedColor::Foreground),
            bg: AnsiColor::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Hash for Style {
    fn hash<H: Hasher>(&self, state: &mut H) {
        color_code(self.fg).hash(state);
        color_code(self.bg).hash(state);
        self.flags.bits().hash(state);
    }
}

/// Rarely-set cell data, boxed so an ordinary cell stays small.
///
/// Hyperlinks are not mirrored: nothing in Allele renders or opens them yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellExtra {
    /// Combining characters drawn over the cell's base character.
    #[serde(rename = "z", default, skip_serializing_if = "Vec::is_empty")]
    pub zerowidth: Vec<char>,
    /// Underline colour as a [`color_code`]; `None` means "same as fg".
    #[serde(rename = "u", default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<u32>,
}

/// One grid cell, in the form the mirror stores and the renderer reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub c: char,
    pub style: Style,
    pub extra: Option<Box<CellExtra>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            style: Style::default(),
            extra: None,
        }
    }
}

impl Cell {
    /// Copy what the mirror needs out of an alacritty cell.
    pub fn from_term(cell: &TermCell) -> Self {
        let zerowidth = cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default();
        let underline_color = cell.underline_color().map(color_code);
        let extra = if zerowidth.is_empty() && underline_color.is_none() {
            None
        } else {
            Some(Box::new(CellExtra {
                zerowidth,
                underline_color,
            }))
        };
        Self {
            c: cell.c,
            style: Style {
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
            },
            extra,
        }
    }

    fn is_blank(&self) -> bool {
        self.c == ' ' && self.style == Style::default() && self.extra.is_none()
    }
}

/// A decoded row: exactly `columns` cells.
pub type Row = Vec<Cell>;

// ── Wire rows ──────────────────────────────────────────────────────

fn default_fg_code() -> u32 {
    color_code(AnsiColor::Named(NamedColor::Foreground))
}

fn default_bg_code() -> u32 {
    color_code(AnsiColor::Named(NamedColor::Background))
}

fn is_default_fg(code: &u32) -> bool {
    *code == default_fg_code()
}

fn is_default_bg(code: &u32) -> bool {
    *code == default_bg_code()
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}

/// A run of consecutive cells that share one style.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// One `char` per cell (wide-char spacers included, so `chars().count()`
    /// is the run's width in cells).
    #[serde(rename = "t")]
    pub text: String,
    #[serde(
        rename = "f",
        default = "default_fg_code",
        skip_serializing_if = "is_default_fg"
    )]
    pub fg: u32,
    #[serde(
        rename = "b",
        default = "default_bg_code",
        skip_serializing_if = "is_default_bg"
    )]
    pub bg: u32,
    /// alacritty `Flags` bits.
    #[serde(rename = "a", default, skip_serializing_if = "is_zero")]
    pub flags: u16,
    /// Extras keyed by cell offset within the run.
    #[serde(rename = "x", default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<(u16, CellExtra)>,
}

/// The encoded form of a [`Row`]: style runs, trailing blank cells dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireRow(pub Vec<Run>);

impl WireRow {
    pub fn encode(row: &[Cell]) -> Self {
        let used = row.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1);
        let mut runs: Vec<Run> = Vec::new();
        let mut current: Option<(Style, Run)> = None;

        for cell in &row[..used] {
            let same = matches!(&current, Some((style, _)) if *style == cell.style);
            if !same {
                if let Some((_, run)) = current.take() {
                    runs.push(run);
                }
                current = Some((
                    cell.style,
                    Run {
                        text: String::new(),
                        fg: color_code(cell.style.fg),
                        bg: color_code(cell.style.bg),
                        flags: cell.style.flags.bits(),
                        extras: Vec::new(),
                    },
                ));
            }
            if let Some((_, run)) = current.as_mut() {
                if let Some(extra) = &cell.extra {
                    let offset = run.text.chars().count() as u16;
                    run.extras.push((offset, (**extra).clone()));
                }
                run.text.push(cell.c);
            }
        }
        if let Some((_, run)) = current {
            runs.push(run);
        }
        WireRow(runs)
    }

    /// Decode into exactly `columns` cells. A row wider than `columns` is a
    /// protocol error, not something to truncate quietly.
    pub fn decode(&self, columns: usize) -> Result<Row> {
        let mut row: Row = Vec::with_capacity(columns);
        for run in &self.0 {
            let style = Style {
                fg: decode_color(run.fg)?,
                bg: decode_color(run.bg)?,
                flags: Flags::from_bits_truncate(run.flags),
            };
            let start = row.len();
            row.extend(run.text.chars().map(|c| Cell {
                c,
                style,
                extra: None,
            }));
            for (offset, extra) in &run.extras {
                let cell = row.get_mut(start + *offset as usize).ok_or_else(|| {
                    AlleleError::Mirror(format!("extra at offset {offset} is past its run"))
                })?;
                cell.extra = Some(Box::new(extra.clone()));
            }
        }
        if row.len() > columns {
            return Err(AlleleError::Mirror(format!(
                "row has {} cells but the grid has {columns} columns",
                row.len()
            )));
        }
        row.resize(columns, Cell::default());
        Ok(row)
    }
}

// ── Cursor ─────────────────────────────────────────────────────────

/// Cursor shape as the grid element draws it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl From<alacritty_terminal::vte::ansi::CursorShape> for CursorShape {
    fn from(shape: alacritty_terminal::vte::ansi::CursorShape) -> Self {
        use alacritty_terminal::vte::ansi::CursorShape as A;
        match shape {
            A::Block => Self::Block,
            A::Underline => Self::Underline,
            A::Beam => Self::Beam,
            A::HollowBlock => Self::HollowBlock,
            A::Hidden => Self::Hidden,
        }
    }
}

/// Cursor position (in screen coordinates, line 0 = top of the screen) and shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cursor {
    pub line: u16,
    pub column: u16,
    pub shape: CursorShape,
}

// ── Frames ─────────────────────────────────────────────────────────

/// The whole visible screen. Sent on attach, on resize, and whenever a diff
/// cannot express the change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// Frame sequence number; the next diff carries `seq + 1`.
    pub seq: u64,
    pub columns: u16,
    pub lines: u16,
    /// Lines of scrollback the host holds. Their content is fetched in pages.
    pub history_size: u32,
    pub cursor: Cursor,
    /// Exactly `lines` rows, top first.
    pub rows: Vec<WireRow>,
}

/// Changes since the previous frame.
///
/// Applied in order: replace each of `scrolled_rows`, scroll the screen up by
/// `scrolled` (the top rows move into scrollback), drop loaded scrollback if
/// `history_reset`, then replace each of `rows`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowDiff {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub scrolled: u16,
    /// `(screen line < scrolled, content)`: rows that changed on the host and
    /// then scrolled off before this frame, as they went into scrollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scrolled_rows: Vec<(u16, WireRow)>,
    /// The host's scrollback changed in a way the scroll count cannot express
    /// (cleared, alt screen, or more than a screenful scrolled at once).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_reset: bool,
    pub history_size: u32,
    pub cursor: Cursor,
    /// `(screen line, new content)`.
    pub rows: Vec<(u16, WireRow)>,
}

/// Scrollback rows `start..start + rows.len()`, where 0 is the line just
/// above the screen. Indices are relative to the frame stream position the
/// page arrives at, which is why the producer sends any pending diff first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackPage {
    pub start: u32,
    pub history_size: u32,
    pub rows: Vec<WireRow>,
}

/// Everything a host sends down one screen stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScreenFrame {
    Snapshot(ScreenSnapshot),
    Diff(RowDiff),
    Scrollback(ScrollbackPage),
}
