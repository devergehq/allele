//! Content column measures for the transcript feed (DEV-571).
//!
//! The feed used to render every block at the full pane width, so prose in a
//! wide window ran to lines the eye cannot reliably return-sweep. Two measures
//! fix that without breaking diff review:
//!
//!   * [`frame_width`] — the width **every** block shares. Blocks are centred
//!     as a group, so their left edges align into a single column rather than
//!     ragging in and out with their content. Wide content (diffs, code,
//!     tables, tool output) gets all of it.
//!   * [`prose_width`] — narrower, applied to running text inside that frame.
//!     Paragraphs, headings and list items wrap here; they sit at the frame's
//!     left edge, so the extra space falls on the right rather than shifting
//!     the text.
//!
//! Both are expressed in **ems**, not pixels. `font_size` is user-adjustable,
//! and the readability guidance these derive from is stated in characters —
//! a fixed pixel measure would let the actual character count drift with the
//! very setting the rule is about.
//!
//! Below the frame width there is no floor: the column simply becomes the pane.
//! A minimum width would introduce horizontal page scrolling, which costs more
//! than a slightly-too-wide measure ever saves.

use gpui::{
    div, px, Div, ElementId, InteractiveElement as _, ParentElement as _, Pixels, Stateful,
    StatefulInteractiveElement as _, Styled as _,
};

/// Running-text measure, in ems.
///
/// ~72 characters at an average glyph width of ~0.5em is 36em; rounded up to
/// 38 because bullets, checkboxes and inline code shorten the effective line.
const PROSE_EM: f32 = 38.0;

/// Shared block frame, in ems.
///
/// Sized so an 80-column monospace line fits without truncation: 80 chars at
/// ~0.6em is 48em, plus the gutters and padding a diff or code block adds.
const FRAME_EM: f32 = 64.0;

/// Maximum width of running text at this font size.
pub fn prose_width(font_size: f32) -> Pixels {
    px(font_size * PROSE_EM)
}

/// Maximum width of the shared block frame at this font size.
pub fn frame_width(font_size: f32) -> Pixels {
    px(font_size * FRAME_EM)
}

/// Centre `inner` inside the shared column frame.
///
/// `w_full().min_w_0()` on the inner div is load-bearing: the feed's virtual
/// list sizes items automatically, and without it a block grows to the
/// intrinsic width of its widest text run (a long diff line, stringified
/// JSON) with no horizontal viewport scroll to recover it.
pub fn framed(font_size: f32, inner: impl gpui::IntoElement) -> Div {
    div().w_full().flex().justify_center().child(
        div()
            .w_full()
            .min_w_0()
            .max_w(frame_width(font_size))
            .child(inner),
    )
}

/// The escape hatch for content the frame is too narrow to hold.
///
/// [`frame_width`] caps how wide a block may be, which is right for prose and
/// wrong for a code line or a diff line — their width is set by their content,
/// and wrapping one does not shorten it, it just breaks it across rows. A
/// wrapped diff line is worse still: the continuation renders without the
/// `+`/`-` that says which side it came from, so an added line can read as a
/// removed one.
///
/// So content like that scrolls sideways inside its own block instead. The page
/// never scrolls horizontally; only the artifact does.
///
/// `id` must be unique among its siblings. GPUI keys the scroll offset by the
/// element's id path and keeps it across frames, so a duplicate id silently
/// makes two blocks share one scroll position.
/// Approximate advance width of one monospace character, as a fraction of the
/// font size. Code fences and diffs are monospace, so a line's width is simply
/// its length times this — which is the one thing that makes [`scroll_x`]
/// usable: GPUI's nowrap text paints past its bounds without reporting a wider
/// measured size, so a scroller wrapping it records a content size equal to its
/// own viewport and decides there is nothing to scroll. An explicit width taken
/// from the longest line gives it a real content size instead.
///
/// Slightly generous on purpose: over-estimating leaves a little dead space at
/// the right end of the scroll, while under-estimating clips the last glyphs.
const MONO_ADVANCE: f32 = 0.62;

/// Width needed to lay out `columns` monospace characters at `font_size`.
pub fn mono_width(columns: usize, font_size: f32) -> Pixels {
    px(columns as f32 * font_size * MONO_ADVANCE)
}

pub fn scroll_x(id: impl Into<ElementId>, inner: Div) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .overflow_x_scroll()
        // The scroller is a flex row, so its child's main axis is horizontal
        // and the default `flex-shrink: 1` would squeeze the content back down
        // to the container. Squeezed content is not overflowing content, and a
        // scroller with nothing to scroll silently clips instead.
        .child(inner.flex_shrink_0())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_width_grows_with_both_columns_and_font_size() {
        assert!(mono_width(80, 13.0) > mono_width(40, 13.0));
        assert!(mono_width(80, 26.0) > mono_width(80, 13.0));
    }

    #[test]
    fn mono_width_is_zero_for_no_columns() {
        assert_eq!(mono_width(0, 13.0), px(0.0));
    }

    #[test]
    fn a_long_code_line_outgrows_the_frame() {
        // The whole reason `scroll_x` exists: at any sane font size a line
        // this long cannot fit the frame, so it must have somewhere to go
        // other than wrapping.
        for font_size in [10.0, 13.0, 16.0] {
            assert!(
                mono_width(120, font_size) > frame_width(font_size),
                "a 120-column line must exceed the frame at font size {font_size}"
            );
        }
    }

    #[test]
    fn a_line_that_fits_the_frame_needs_no_scrolling() {
        // Guards the other direction: short content must not acquire a
        // scrollable extent it does not need.
        for font_size in [10.0, 13.0, 16.0] {
            assert!(mono_width(40, font_size) < frame_width(font_size));
        }
    }

    #[test]
    fn prose_is_narrower_than_the_frame() {
        for font_size in [10.0, 13.0, 16.0, 24.0] {
            assert!(
                prose_width(font_size) < frame_width(font_size),
                "prose must fit inside the frame at font size {font_size}"
            );
        }
    }

    #[test]
    fn measures_scale_with_font_size() {
        // The measures are stated in characters, so they must track the font
        // size rather than pinning a fixed pixel width.
        assert_eq!(prose_width(20.0), prose_width(10.0) * 2.0);
        assert_eq!(frame_width(20.0), frame_width(10.0) * 2.0);
    }

    #[test]
    fn prose_measure_lands_in_the_readable_range() {
        // ~0.5em average glyph width, so em-count / 0.5 approximates the
        // character count the measure yields. Guard the typographic intent,
        // not the constant.
        let chars = PROSE_EM / 0.5;
        assert!(
            (60.0..=90.0).contains(&chars),
            "prose measure should land near the 65-75 character range, got {chars}"
        );
    }
}
