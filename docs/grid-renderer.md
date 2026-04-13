# Cell Grid Renderer

Technical reference for `src/terminal/grid_element.rs` — the `TerminalGridElement` GPUI Element.

## Why a Custom Element

The initial div-based terminal rendering didn't enforce monospace alignment. GPUI's text layout compresses glyphs, causing stretched output, misaligned box-drawing, incorrect column counts, and broken scrollback rendering. The fix: a custom `gpui::Element` that paints characters at exact pixel coordinates in a fixed-width cell grid.

## The Cell Grid Model

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
```

## The Rendering Pipeline

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
   Paint selection highlight (if active)
   Paint search match highlights (if active)
   Paint text: shaped_line.paint() at exact cell coordinates
   Paint cursor: quad at cursor cell position
   Paint URL underline (if hovering)
   Paint scrollbar (if history exists)
```

## Element Implementation

`TerminalGridElement` implements `gpui::Element` with three phases:

### `request_layout()`

Returns the desired size as `cols × cell_width` by `rows × cell_height`. Cell dimensions are measured from the font's advance width:

```rust
let advance = window.text_system().advance(font_id, font_size, 'm');
let cell_width = advance.width;
let cell_height = (font_size * 1.385).ceil();
```

This is the key insight: when we position text ourselves (not using GPUI's text flow), the advance width is the correct cell width.

### `prepaint()`

Reads the alacritty terminal grid and prepares paint data:

1. **Background spans** — consecutive cells with the same bg colour are batched into `BgSpan` structs (col_start, col_end, colour). Default background cells are skipped.
2. **Text spans** — consecutive cells with the same fg colour + bold/italic flags are batched into `TextSpan` structs. Each span is shaped via `shape_line()` with the correct font variant.
3. **Cursor position** — tracked during the cell iteration pass.
4. **Scroll offset** — `display_offset` from alacritty's grid translates screen rows to history rows.

### `paint()`

Paints in strict Z-order:
1. Background quads (per `BgSpan`)
2. Selection highlight overlay (blue, 50% alpha)
3. Search match highlights (orange for current, yellow for others)
4. Text (per `TextSpan`, positioned at exact cell coordinates)
5. Non-block cursor shapes (beam, underline, hollow block)
6. URL underline on hover
7. Scrollbar track + thumb (fades in/out based on scroll activity)

Block cursor is handled during prepaint by inverting the cell's fg/bg colours.

## Cursor Shapes

| Shape | Rendering |
|-------|-----------|
| Block | Fg/bg colour inversion during prepaint |
| Beam | 2px vertical bar at left edge of cell |
| Underline | 2px horizontal bar at bottom of cell |
| HollowBlock | 1px border rectangle |
| Hidden | Not rendered |

Cursor blinks with a configurable interval, managed by `TerminalView`.

## Colour Scheme

Catppuccin Mocha palette, hardcoded in `ansi_to_hsla()`. Supports:
- 16 named ANSI colours
- 256-colour indexed palette (6×6×6 RGB cube + 24 greyscale ramp)
- 24-bit truecolor via `AnsiColor::Spec`

## Reference: Termy's Approach

Termy's `grid.rs` (~3500 lines) does the same thing with additional optimisations we haven't implemented:
- Row-level paint cache (only re-render rows that changed)
- Damage tracking (Full, Rows, RowRanges with column granularity)
- Unicode block elements (U+2580-U+259F) rendered as pixel-snapped quads
- Box-drawing (U+2500-U+257F) rendered with Ghostty-style sprite geometry
- Rounded corners (U+256D-U+2570) use cubic Bezier paths
- ShapedLine objects cached and reused across frames

These are optimisation and polish opportunities for the future.
