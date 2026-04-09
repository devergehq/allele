# Cell Grid Renderer — Implementation Plan

**Goal:** Replace div-based terminal rendering with a proper GPUI Element that renders a fixed-width character cell grid, matching how Terminal.app, iTerm2, Alacritty, and Termy render terminal output.

**Why:** The div-based approach doesn't enforce monospace alignment. GPUI's text layout compresses glyphs, causing stretched output, misaligned box-drawing, incorrect column counts, and broken scrollback rendering.

---

## How It Works

### The Cell Grid Model

```
┌──────┬──────┬──────┬──────┬──────┬──────┐
│  H   │  e   │  l   │  l   │  o   │      │  ← Row 0
├──────┼──────┼──────┼──────┼──────┼──────┤
│  ─   │  ─   │  ─   │  ─   │  ─   │  ─   │  ← Row 1 (box drawing)
├──────┼──────┼──────┼──────┼──────┼──────┤
│  W   │  o   │  r   │  l   │  d   │      │  ← Row 2
└──────┴──────┴──────┴──────┴──────┴──────┘
  col0   col1   col2   col3   col4   col5

Every cell = cell_width × cell_height pixels
Characters are shaped and positioned at exact pixel coordinates
Box-drawing characters rendered as geometric primitives spanning cell boundaries
```

### The Rendering Pipeline

```
alacritty_terminal grid
        │
        ▼
   Read each cell: char, fg, bg, flags
        │
        ▼
   For each row, batch consecutive cells with same style
        │
        ▼
   Shape text batches via window.text_system().shape_line()
        │
        ▼
   Paint backgrounds: window.paint_quad() at exact cell coordinates
   Paint text: shaped_line.paint() at exact cell coordinates  
   Paint cursor: window.paint_quad() at cursor cell position
```

---

## Implementation Steps

### Step 1: Create TerminalElement struct

A new struct that implements `gpui::Element`. This replaces the div-based rendering in `terminal_view.rs`.

```rust
pub struct TerminalElement {
    term: Arc<FairMutex<Term<JsonEventListener>>>,
    cell_width: Pixels,
    cell_height: Pixels,
    font: Font,
    font_size: Pixels,
}
```

**Implements:**
- `Element::request_layout()` — returns the desired size (cols × cell_width, rows × cell_height)
- `Element::prepaint()` — read grid, batch cells by style, shape text
- `Element::paint()` — paint backgrounds, text, cursor at exact positions

### Step 2: Measure cell dimensions properly

During Element layout, measure the actual advance width of a character in the configured font using GPUI's text system. Since we're painting at exact coordinates (not relying on GPUI's text flow), the advance width IS the correct cell width.

```rust
let advance = window.text_system().advance(font_id, font_size, 'M')?;
let cell_width = advance.width;
```

This is the key difference from the div approach — when we position text ourselves, the advance width is correct. It was wrong before because GPUI's div layout was compressing the glyphs.

### Step 3: Paint backgrounds

For each row, iterate cells and batch consecutive cells with the same background colour. Paint each batch as a single quad:

```rust
window.paint_quad(PaintQuad {
    bounds: Bounds::new(
        point(col_start * cell_width, row * cell_height),
        size(span_width, cell_height),
    ),
    background: bg_color,
    ..Default::default()
});
```

### Step 4: Paint text

For each row, batch consecutive cells with the same foreground style (colour, bold, italic). Shape each batch via `shape_line()`, then paint at the exact pixel position:

```rust
let shaped = window.text_system().shape_line(
    text.into(),
    font_size,
    &[TextRun { len: text.len(), font, color: fg, .. }],
    None,
);
shaped.paint(point(col_start * cell_width, row * cell_height), line_height, window);
```

### Step 5: Paint cursor

Paint a quad at the cursor position. For a block cursor, it's a filled rectangle the size of one cell with the foreground colour, and the character under it painted in the background colour.

### Step 6: Handle scroll

The scroll handler calls `term.scroll_display()` which changes which rows alacritty returns for Line(0)..Line(N). The Element reads whatever the grid currently shows — scroll is handled by alacritty, not by us.

### Step 7: Wire into TerminalView

Replace the current div-building render code with the new TerminalElement:

```rust
impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ... keyboard handling stays the same ...
        
        div()
            .id("terminal")
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(/* ... same as now ... */)
            .on_scroll_wheel(/* ... same as now ... */)
            .child(TerminalElement::new(
                self.terminal.term.clone(),
                cell_width,
                cell_height,
                font,
                font_size,
            ))
    }
}
```

---

## What This Fixes

| Issue | Why it's fixed |
|-------|---------------|
| Text stretching | Characters positioned at exact grid coordinates, not GPUI text flow |
| Box-drawing misalignment | Each char occupies exactly one cell width regardless of glyph shape |
| Wrong column count | Cell width is the font's advance width, which is correct for positioned rendering |
| Status line too wide | Correct column count = correct line wrapping |
| Scrollback | Grid reads whatever display offset alacritty has — scroll just works |

## What This Doesn't Fix (separate concerns)

- Sidebar jumping (UI layout issue, not terminal rendering)
- API content filtering (Anthropic's API, nothing to do with us)

---

## Reference: Termy's Approach

Termy's `grid.rs` (~3500 lines) does exactly this but with additional optimisations:
- Row-level paint cache (only re-render rows that changed)
- Damage tracking (Full, Rows, RowRanges with column granularity)
- Unicode block elements (U+2580-U+259F) rendered as pixel-snapped quads
- Box-drawing (U+2500-U+257F) rendered with Ghostty-style sprite geometry
- Rounded corners (U+256D-U+2570) use cubic Bezier paths
- ShapedLine objects cached and reused across frames

For our initial implementation, we skip the caching and fancy box-drawing and just do:
1. Paint all backgrounds
2. Shape and paint all text
3. Paint cursor

This gives us correct alignment. Caching and box-drawing sprites are optimisation and polish for later.

---

## Estimated Effort

| Step | Effort |
|------|--------|
| 1. TerminalElement struct + Element trait impl | 1-2 days |
| 2. Cell dimension measurement | Done (advance width is correct for positioned rendering) |
| 3. Background painting | Half day |
| 4. Text shaping + painting | 1 day |
| 5. Cursor painting | Half day |
| 6. Scroll wiring | Already done |
| 7. Integration into TerminalView | Half day |
| **Total** | **~3-4 days** |

## Files Changed

- `src/terminal/grid_element.rs` — NEW: the TerminalElement implementation
- `src/terminal/terminal_view.rs` — MODIFIED: replace div rendering with TerminalElement
- `src/terminal/mod.rs` — MODIFIED: add grid_element module

No changes needed to: pty_terminal.rs, session/mod.rs, main.rs, clone/mod.rs
