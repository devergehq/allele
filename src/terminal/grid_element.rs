use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use gpui::*;
use std::sync::Arc;

use super::pty_terminal::JsonEventListener;

/// A GPUI Element that renders a terminal cell grid.
///
/// Each character occupies exactly one cell (cell_width × cell_height pixels).
/// Characters are shaped and painted at exact pixel coordinates, bypassing
/// GPUI's text flow layout which compresses monospace glyphs.
/// Selection range in alacritty line coordinates.
/// `anchor` and `extent` are (line_offset, col) where line_offset is
/// alacritty's Line value (negative for history, 0..screen_lines for visible).
#[derive(Clone, Copy)]
pub struct GridSelection {
    pub anchor: (i32, usize),
    pub extent: (i32, usize),
}

pub struct TerminalGridElement {
    term: Arc<FairMutex<Term<JsonEventListener>>>,
    font: Font,
    font_size: Pixels,
    cell_width: Pixels,
    cell_height: Pixels,
    cursor_visible: bool,
    scrollbar_opacity: f32,
    selection: Option<GridSelection>,
    search_matches: Vec<(i32, usize, usize)>, // (line_offset, col_start, col_end) — alacritty Line value
    search_current_idx: usize,
    hovered_url: Option<(usize, usize, usize)>, // (row, col_start, col_end)
    // Shared atomic origin — updated during paint so mouse handlers can translate coords
    origin_x_out: Option<Arc<std::sync::atomic::AtomicI32>>,
    origin_y_out: Option<Arc<std::sync::atomic::AtomicI32>>,
}

impl TerminalGridElement {
    pub fn new(
        term: Arc<FairMutex<Term<JsonEventListener>>>,
        font: Font,
        font_size: Pixels,
        cell_width: Pixels,
        cell_height: Pixels,
    ) -> Self {
        Self {
            term,
            font,
            font_size,
            cell_width,
            cell_height,
            cursor_visible: true,
            scrollbar_opacity: 0.0,
            selection: None,
            search_matches: Vec::new(),
            search_current_idx: 0,
            hovered_url: None,
            origin_x_out: None,
            origin_y_out: None,
        }
    }

    pub fn cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    pub fn scrollbar_opacity(mut self, opacity: f32) -> Self {
        self.scrollbar_opacity = opacity;
        self
    }

    pub fn selection(mut self, sel: Option<GridSelection>) -> Self {
        self.selection = sel;
        self
    }

    pub fn search_matches(mut self, matches: Vec<(i32, usize, usize)>, current_idx: usize) -> Self {
        self.search_matches = matches;
        self.search_current_idx = current_idx;
        self
    }

    pub fn hovered_url(mut self, url: Option<(usize, usize, usize)>) -> Self {
        self.hovered_url = url;
        self
    }

    pub fn origin_out(
        mut self,
        x: Arc<std::sync::atomic::AtomicI32>,
        y: Arc<std::sync::atomic::AtomicI32>,
    ) -> Self {
        self.origin_x_out = Some(x);
        self.origin_y_out = Some(y);
        self
    }

    /// Measure cell dimensions from the font. Call once and reuse.
    pub fn measure_cell(window: &Window, font: &Font, font_size: Pixels) -> (Pixels, Pixels) {
        let font_id = window.text_system().resolve_font(font);
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.0));
        let cell_height = px((f32::from(font_size) * 1.385).ceil());
        (cell_width, cell_height)
    }
}

/// State computed during request_layout, passed to prepaint and paint.
#[allow(dead_code)]
pub struct GridLayoutState {
    cell_width: Pixels,
    cell_height: Pixels,
    cols: usize,
    rows: usize,
}

/// Prepared row data for painting.
struct PreparedRow {
    bg_spans: Vec<BgSpan>,
    text_spans: Vec<TextSpan>,
}

struct BgSpan {
    col_start: usize,
    col_end: usize, // exclusive
    color: Hsla,
}

struct TextSpan {
    col_start: usize,
    shaped: ShapedLine,
}

/// Cursor shape read from alacritty's grid state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

/// State computed during prepaint, passed to paint.
pub struct GridPrepaintState {
    rows: Vec<PreparedRow>,
    cursor_point: Option<(usize, usize)>, // (row, col) in visible coords
    cursor_shape: CursorShape,
    // Scroll state for scrollbar
    display_offset: usize,
    total_lines: usize,
    screen_lines: usize,
}

impl IntoElement for TerminalGridElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalGridElement {
    type RequestLayoutState = GridLayoutState;
    type PrepaintState = GridPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;

        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        drop(term);

        let total_width = cell_width * cols as f32;
        let total_height = cell_height * rows as f32;

        // Request a fixed-size layout
        let mut style = Style::default();
        style.size.width = length(total_width);
        style.size.height = length(total_height);

        let layout_id = window.request_layout(style, [], cx);

        let state = GridLayoutState {
            cell_width,
            cell_height,
            cols,
            rows,
        };

        (layout_id, state)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        layout_state: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {

        let term = self.term.lock();
        let cursor_shape_raw = term.cursor_style().shape;
        let grid = term.grid();
        let cursor_point = grid.cursor.point;
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        let display_offset = grid.display_offset() as i32;

        let cursor_shape = match cursor_shape_raw {
            alacritty_terminal::vte::ansi::CursorShape::Block => CursorShape::Block,
            alacritty_terminal::vte::ansi::CursorShape::Underline => CursorShape::Underline,
            alacritty_terminal::vte::ansi::CursorShape::Beam => CursorShape::Beam,
            alacritty_terminal::vte::ansi::CursorShape::HollowBlock => CursorShape::HollowBlock,
            alacritty_terminal::vte::ansi::CursorShape::Hidden => CursorShape::Hidden,
        };

        let default_bg = ansi_to_hsla(&AnsiColor::Named(NamedColor::Background));
        let default_fg = ansi_to_hsla(&AnsiColor::Named(NamedColor::Foreground));

        let mut prepared_rows = Vec::with_capacity(num_lines);
        let mut cursor_pos: Option<(usize, usize)> = None;

        for line_idx in 0..num_lines {
            // Account for scroll: negative Line values access history.
            // When display_offset=5, screen line 0 maps to Line(-5).
            let grid_line = Line(line_idx as i32 - display_offset);
            let mut bg_spans: Vec<BgSpan> = Vec::new();
            let mut text_spans: Vec<TextSpan> = Vec::new();

            // First pass: collect background spans
            let mut bg_start = 0usize;
            let mut bg_color = default_bg;

            for col_idx in 0..num_cols {
                let cell = &grid[grid_line][Column(col_idx)];
                // Track cursor position regardless of shape/visibility for paint pass.
                let at_cursor = display_offset == 0
                    && line_idx == cursor_point.line.0 as usize
                    && col_idx == cursor_point.column.0;
                if at_cursor {
                    cursor_pos = Some((line_idx, col_idx));
                }
                // Only invert cell bg for block cursor shape (when visible)
                let is_block_cursor = at_cursor
                    && self.cursor_visible
                    && cursor_shape == CursorShape::Block;

                let cell_bg = if is_block_cursor {
                    ansi_to_hsla(&AnsiColor::Named(NamedColor::Foreground))
                } else {
                    ansi_to_hsla(&cell.bg)
                };

                if col_idx == 0 {
                    bg_color = cell_bg;
                    bg_start = 0;
                } else if cell_bg != bg_color {
                    // Flush previous bg span (only if non-default)
                    if bg_color != default_bg {
                        bg_spans.push(BgSpan {
                            col_start: bg_start,
                            col_end: col_idx,
                            color: bg_color,
                        });
                    }
                    bg_color = cell_bg;
                    bg_start = col_idx;
                }
            }
            // Flush final bg span
            if bg_color != default_bg {
                bg_spans.push(BgSpan {
                    col_start: bg_start,
                    col_end: num_cols,
                    color: bg_color,
                });
            }

            // Second pass: collect text spans (batched by fg colour + style)
            let mut text_buf = String::new();
            let mut span_start = 0usize;
            let mut span_fg = default_fg;
            let mut span_bold = false;
            let mut span_italic = false;

            for col_idx in 0..num_cols {
                let cell = &grid[grid_line][Column(col_idx)];

                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let is_block_cursor = self.cursor_visible
                    && cursor_shape == CursorShape::Block
                    && display_offset == 0
                    && line_idx == cursor_point.line.0 as usize
                    && col_idx == cursor_point.column.0;

                let cell_fg = if is_block_cursor {
                    ansi_to_hsla(&AnsiColor::Named(NamedColor::Background))
                } else {
                    ansi_to_hsla(&cell.fg)
                };
                let bold = cell.flags.contains(CellFlags::BOLD);
                let italic = cell.flags.contains(CellFlags::ITALIC);

                // Style changed — flush current span
                if col_idx > 0
                    && (cell_fg != span_fg || bold != span_bold || italic != span_italic)
                {
                    if !text_buf.is_empty() {
                        let text: SharedString = std::mem::take(&mut text_buf).into();
                        let font = self.make_font(span_bold, span_italic);
                        let shaped = window.text_system().shape_line(
                            text.clone(),
                            self.font_size,
                            &[TextRun {
                                len: text.len(),
                                font,
                                color: span_fg,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            Some(layout_state.cell_width),
                        );
                        text_spans.push(TextSpan {
                            col_start: span_start,
                            shaped,
                        });
                    }
                    span_start = col_idx;
                    span_fg = cell_fg;
                    span_bold = bold;
                    span_italic = italic;
                }

                if col_idx == 0 {
                    span_fg = cell_fg;
                    span_bold = bold;
                    span_italic = italic;
                }

                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                text_buf.push(ch);
            }

            // Flush final text span
            if !text_buf.is_empty() {
                let text: SharedString = text_buf.into();
                let font = self.make_font(span_bold, span_italic);
                let shaped = window.text_system().shape_line(
                    text.clone(),
                    self.font_size,
                    &[TextRun {
                        len: text.len(),
                        font,
                        color: span_fg,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    Some(layout_state.cell_width),
                );
                text_spans.push(TextSpan {
                    col_start: span_start,
                    shaped,
                });
            }

            prepared_rows.push(PreparedRow {
                bg_spans,
                text_spans,
            });
        }

        let total = grid.total_lines();
        let screen = grid.screen_lines();
        let offset = grid.display_offset();
        drop(term);

        GridPrepaintState {
            rows: prepared_rows,
            cursor_point: cursor_pos,
            cursor_shape,
            display_offset: offset,
            total_lines: total,
            screen_lines: screen,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout_state: &mut Self::RequestLayoutState,
        prepaint_state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let cell_w = layout_state.cell_width;
        let cell_h = layout_state.cell_height;
        let origin = bounds.origin;

        // Publish origin so mouse handlers can translate window coordinates.
        if let (Some(ox), Some(oy)) = (&self.origin_x_out, &self.origin_y_out) {
            ox.store(f32::from(origin.x) as i32, std::sync::atomic::Ordering::Relaxed);
            oy.store(f32::from(origin.y) as i32, std::sync::atomic::Ordering::Relaxed);
        }

        // Paint backgrounds
        for (row_idx, row) in prepaint_state.rows.iter().enumerate() {
            let y = origin.y + cell_h * row_idx as f32;

            for span in &row.bg_spans {
                let x = origin.x + cell_w * span.col_start as f32;
                let w = cell_w * (span.col_end - span.col_start) as f32;
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(w, cell_h)),
                    span.color,
                ));
            }
        }

        // Paint selection highlight.
        // Selection is stored in alacritty line coordinates, so it scrolls
        // with the content — not with the screen.
        if let Some(sel) = &self.selection {
            let (start_line, start_col, end_line, end_col) = normalise_selection(
                sel.anchor, sel.extent,
            );
            let sel_bg = hsla(210.0 / 360.0, 0.6, 0.4, 0.5);
            let display_offset = prepaint_state.display_offset as i32;
            let num_rows = prepaint_state.rows.len() as i32;
            let num_cols = layout_state.cols;

            // Iterate over the line range, translating each to visible row.
            for line in start_line..=end_line {
                let visible_row = line + display_offset;
                if visible_row < 0 || visible_row >= num_rows { continue; }
                let row_idx = visible_row as usize;
                let y = origin.y + cell_h * row_idx as f32;
                let col_start = if line == start_line { start_col } else { 0 };
                let col_end = if line == end_line { end_col + 1 } else { num_cols };
                let x = origin.x + cell_w * col_start as f32;
                let w = cell_w * (col_end - col_start) as f32;
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(w, cell_h)),
                    sel_bg,
                ));
            }
        }

        // Paint search match highlights.
        // Matches are stored as alacritty line offsets (negative = history, 0+ = screen).
        // Translate to visible row index using display_offset.
        let display_offset = prepaint_state.display_offset as i32;
        let num_rows = prepaint_state.rows.len() as i32;
        for (i, &(line_offset, col_start, col_end)) in self.search_matches.iter().enumerate() {
            // visible_row = line_offset + display_offset, but only if 0..num_rows
            let visible_row = line_offset + display_offset;
            if visible_row >= 0 && visible_row < num_rows {
                let row_idx = visible_row as usize;
                let y = origin.y + cell_h * row_idx as f32;
                let x = origin.x + cell_w * col_start as f32;
                let w = cell_w * (col_end - col_start + 1) as f32;
                let bg = if i == self.search_current_idx {
                    hsla(30.0 / 360.0, 0.8, 0.5, 0.6) // current match: orange
                } else {
                    hsla(50.0 / 360.0, 0.7, 0.5, 0.3) // other matches: yellow
                };
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(w, cell_h)),
                    bg,
                ));
            }
        }

        // Paint text
        for (row_idx, row) in prepaint_state.rows.iter().enumerate() {
            let y = origin.y + cell_h * row_idx as f32;

            for span in &row.text_spans {
                let x = origin.x + cell_w * span.col_start as f32;
                let _ = span.shaped.paint(
                    point(x, y),
                    cell_h,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        // Paint non-block cursor shapes (beam/underline/hollow-block)
        if self.cursor_visible {
            if let Some((cursor_row, cursor_col)) = prepaint_state.cursor_point {
                let cursor_color = ansi_to_hsla(&AnsiColor::Named(NamedColor::Foreground));
                let cx_x = origin.x + cell_w * cursor_col as f32;
                let cy_y = origin.y + cell_h * cursor_row as f32;
                match prepaint_state.cursor_shape {
                    CursorShape::Block => {
                        // Already painted via cell bg inversion
                    }
                    CursorShape::Beam => {
                        // Vertical bar at left edge of cell
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x, cy_y), size(px(2.0), cell_h)),
                            cursor_color,
                        ));
                    }
                    CursorShape::Underline => {
                        // Horizontal bar at bottom of cell
                        let bar_h = px(2.0);
                        let y = cy_y + cell_h - bar_h;
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x, y), size(cell_w, bar_h)),
                            cursor_color,
                        ));
                    }
                    CursorShape::HollowBlock => {
                        // 1px border rectangle
                        let bw = px(1.0);
                        // Top
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x, cy_y), size(cell_w, bw)),
                            cursor_color,
                        ));
                        // Bottom
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x, cy_y + cell_h - bw), size(cell_w, bw)),
                            cursor_color,
                        ));
                        // Left
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x, cy_y), size(bw, cell_h)),
                            cursor_color,
                        ));
                        // Right
                        window.paint_quad(fill(
                            Bounds::new(point(cx_x + cell_w - bw, cy_y), size(bw, cell_h)),
                            cursor_color,
                        ));
                    }
                    CursorShape::Hidden => {}
                }
            }
        }

        // Paint URL underline on hover
        if let Some((row, col_start, col_end)) = self.hovered_url {
            if row < prepaint_state.rows.len() {
                let y = origin.y + cell_h * (row as f32 + 1.0) - px(1.5);
                let x = origin.x + cell_w * col_start as f32;
                let w = cell_w * (col_end - col_start + 1) as f32;
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(w, px(1.0))),
                    hsla(210.0 / 360.0, 0.8, 0.7, 0.8),
                ));
            }
        }

        // Paint scrollbar when there's history to scroll through
        let total = prepaint_state.total_lines;
        let screen = prepaint_state.screen_lines;
        let history = total.saturating_sub(screen);

        if history > 0 && self.scrollbar_opacity > 0.01 {
            let scrollbar_width = px(6.0);
            let scrollbar_margin = px(2.0);
            let viewport_height = bounds.size.height;

            // Thumb size: proportion of visible content to total content
            let thumb_ratio = screen as f32 / total as f32;
            let thumb_height = (viewport_height * thumb_ratio).max(px(20.0));

            // Thumb position: display_offset=0 means at bottom (most recent),
            // display_offset=history means at top (oldest)
            let scroll_fraction = prepaint_state.display_offset as f32 / history as f32;
            // Invert: offset=0 → thumb at bottom, offset=max → thumb at top
            let thumb_y = origin.y
                + (viewport_height - thumb_height) * (1.0 - scroll_fraction);

            let track_x = origin.x + bounds.size.width - scrollbar_width - scrollbar_margin;

            // Track (subtle background)
            window.paint_quad(
                quad(
                    Bounds::new(
                        point(track_x, origin.y),
                        size(scrollbar_width, viewport_height),
                    ),
                    px(3.0), // corner radius
                    hsla(0.0, 0.0, 1.0, 0.05 * self.scrollbar_opacity), // very subtle track
                    px(0.0),
                    hsla(0.0, 0.0, 0.0, 0.0),
                    BorderStyle::default(),
                ),
            );

            // Thumb
            window.paint_quad(
                quad(
                    Bounds::new(
                        point(track_x, thumb_y),
                        size(scrollbar_width, thumb_height),
                    ),
                    px(3.0), // corner radius
                    hsla(0.0, 0.0, 1.0, 0.3 * self.scrollbar_opacity), // visible but not distracting
                    px(0.0),
                    hsla(0.0, 0.0, 0.0, 0.0),
                    BorderStyle::default(),
                ),
            );
        }
    }
}

impl TerminalGridElement {
    /// Build a Font with the appropriate weight/style for a span.
    fn make_font(&self, bold: bool, italic: bool) -> Font {
        Font {
            weight: if bold {
                FontWeight::BOLD
            } else {
                self.font.weight
            },
            style: if italic {
                FontStyle::Italic
            } else {
                self.font.style
            },
            ..self.font.clone()
        }
    }
}

// ── Colour conversion ──────────────────────────────────────────────

fn ansi_to_hsla(color: &AnsiColor) -> Hsla {
    let rgba_val: u32 = match color {
        AnsiColor::Named(named) => match named {
            NamedColor::Black => 0x45475a,
            NamedColor::Red => 0xf38ba8,
            NamedColor::Green => 0xa6e3a1,
            NamedColor::Yellow => 0xf9e2af,
            NamedColor::Blue => 0x89b4fa,
            NamedColor::Magenta => 0xcba6f7,
            NamedColor::Cyan => 0x94e2d5,
            NamedColor::White => 0xbac2de,
            NamedColor::BrightBlack => 0x585b70,
            NamedColor::BrightRed => 0xf38ba8,
            NamedColor::BrightGreen => 0xa6e3a1,
            NamedColor::BrightYellow => 0xf9e2af,
            NamedColor::BrightBlue => 0x89b4fa,
            NamedColor::BrightMagenta => 0xcba6f7,
            NamedColor::BrightCyan => 0x94e2d5,
            NamedColor::BrightWhite => 0xffffff,
            NamedColor::Foreground => 0xcdd6f4,
            NamedColor::Background => 0x1e1e2e,
            _ => 0xcdd6f4,
        },
        AnsiColor::Spec(rgb_color) => {
            return Hsla::from(Rgba {
                r: rgb_color.r as f32 / 255.0,
                g: rgb_color.g as f32 / 255.0,
                b: rgb_color.b as f32 / 255.0,
                a: 1.0,
            });
        }
        AnsiColor::Indexed(idx) => {
            match *idx {
                0 => 0x45475a,
                1 => 0xf38ba8,
                2 => 0xa6e3a1,
                3 => 0xf9e2af,
                4 => 0x89b4fa,
                5 => 0xcba6f7,
                6 => 0x94e2d5,
                7 => 0xbac2de,
                8 => 0x585b70,
                9 => 0xf38ba8,
                10 => 0xa6e3a1,
                11 => 0xf9e2af,
                12 => 0x89b4fa,
                13 => 0xcba6f7,
                14 => 0x94e2d5,
                15 => 0xffffff,
                16..=231 => {
                    let i = idx - 16;
                    let r = (i / 36) * 51;
                    let g = ((i / 6) % 6) * 51;
                    let b = (i % 6) * 51;
                    return Hsla::from(Rgba {
                        r: r as f32 / 255.0,
                        g: g as f32 / 255.0,
                        b: b as f32 / 255.0,
                        a: 1.0,
                    });
                }
                _ => {
                    let v = (idx - 232) * 10 + 8;
                    return Hsla::from(Rgba {
                        r: v as f32 / 255.0,
                        g: v as f32 / 255.0,
                        b: v as f32 / 255.0,
                        a: 1.0,
                    });
                }
            }
        }
    };
    let r = ((rgba_val >> 16) & 0xFF) as f32 / 255.0;
    let g = ((rgba_val >> 8) & 0xFF) as f32 / 255.0;
    let b = (rgba_val & 0xFF) as f32 / 255.0;
    Hsla::from(Rgba { r, g, b, a: 1.0 })
}

/// Normalise selection so start <= end (top-left to bottom-right).
fn normalise_selection(
    anchor: (i32, usize),
    extent: (i32, usize),
) -> (i32, usize, i32, usize) {
    if anchor.0 < extent.0 || (anchor.0 == extent.0 && anchor.1 <= extent.1) {
        (anchor.0, anchor.1, extent.0, extent.1)
    } else {
        (extent.0, extent.1, anchor.0, anchor.1)
    }
}

/// Helper to create a DefiniteLength from Pixels for style sizing.
fn length(px_val: Pixels) -> Length {
    Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px_val)))
}
