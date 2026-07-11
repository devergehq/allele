//! Semantic design tokens for Allele's chrome (UI refresh phase 1, DEV-2).
//!
//! Every chrome color flows through a named role on [`Theme`] instead of a
//! raw hex literal, so a future light theme is a palette swap rather than a
//! rewrite. Values are Catppuccin Mocha, preserved exactly from the
//! pre-token UI — this module is plumbing, not a redesign.
//!
//! The terminal's ANSI palette (src/terminal/grid_element.rs) is content,
//! not chrome, and deliberately does not use these tokens.

use gpui::{hsla, rgb, rgba, Hsla};
use std::sync::OnceLock;

/// UI chrome font — resolves to the macOS system font (SF Pro) via GPUI's
/// `.SystemUIFont` fallback.
pub const FONT_UI: &str = ".SystemUIFont";
/// Content/code font — terminal grid, diffs, code blocks, tool payloads.
pub const FONT_MONO: &str = "JetBrains Mono";

/// Type scale (px). Chrome floor is `TEXT_SM`; `TEXT_XS` is reserved for
/// icon-adjacent glyphs and dense metadata that has explicit sign-off.
/// Adopted incrementally as surfaces are touched (phases 3-4).
#[allow(dead_code)]
pub const TEXT_XS: f32 = 11.0;
#[allow(dead_code)]
pub const TEXT_SM: f32 = 12.0;
#[allow(dead_code)]
pub const TEXT_BASE: f32 = 13.0;
#[allow(dead_code)]
pub const TEXT_LG: f32 = 15.0;
#[allow(dead_code)]
pub const TEXT_XL: f32 = 17.0;

/// Radius scale (px): SM for small inline controls, MD for buttons/cards,
/// LG for modals and floating panels.
#[allow(dead_code)]
pub const RADIUS_SM: f32 = 4.0;
#[allow(dead_code)]
pub const RADIUS_MD: f32 = 6.0;
#[allow(dead_code)]
pub const RADIUS_LG: f32 = 10.0;

/// Semantic color roles for the app chrome. All values are `Hsla` so they
/// slot into any GPUI position (`bg`, `text_color`, `border_color`,
/// `TextRun.color`, …) without conversion noise.
pub struct Theme {
    // ── Backgrounds (layered elevation) ──────────────────────────
    /// Main content background (Mocha base).
    pub bg_base: Hsla,
    /// Sidebar / panel background (mantle).
    pub bg_surface: Hsla,
    /// Input wells, project headers — below surface (crust).
    pub bg_sunken: Hsla,
    /// Selected rows, buttons, chips (surface0).
    pub bg_raised: Hsla,
    /// Hover fills (surface1).
    pub bg_hover: Hsla,
    /// Pressed / stronger emphasis fills (surface2).
    pub bg_active: Hsla,
    /// Soft hover for large rows.
    pub bg_hover_soft: Hsla,
    /// Alternating row background.
    pub bg_row_alt: Hsla,
    /// Attention-bar background (purple tint).
    pub bg_attention: Hsla,
    /// Terminal bell flash.
    pub bg_bell: Hsla,

    // ── State tints (background washes) ──────────────────────────
    pub tint_danger: Hsla,
    pub tint_danger_hover: Hsla,
    pub tint_danger_soft: Hsla,
    pub tint_warning: Hsla,
    pub tint_warning_hover: Hsla,
    pub tint_warning_soft: Hsla,

    // ── Text ─────────────────────────────────────────────────────
    pub text_primary: Hsla,
    /// Slightly softer body text (subtext1).
    pub text_body: Hsla,
    /// Secondary labels (subtext0).
    pub text_secondary: Hsla,
    /// Muted labels (overlay2).
    pub text_muted: Hsla,
    /// Faint hints, inactive glyphs (overlay0).
    pub text_faint: Hsla,
    /// Dimmest legible text — timestamps, ages (surface2).
    pub text_dim: Hsla,
    /// Ghost — idle icon buttons that light up on hover (surface1).
    pub text_ghost: Hsla,
    /// Dark text on colored (accent/success/…) fills.
    pub text_on_accent: Hsla,
    /// Input placeholder text.
    pub text_placeholder: Hsla,

    // ── Borders ──────────────────────────────────────────────────
    pub border_subtle: Hsla,
    pub border_default: Hsla,
    pub border_strong: Hsla,

    // ── Accents ──────────────────────────────────────────────────
    /// Primary interactive accent (blue).
    pub accent: Hsla,
    /// Informational (sapphire).
    pub info: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    /// Softer danger for secondary marks (maroon).
    pub danger_soft: Hsla,
    /// Urgent attention — session blocked on input (peach).
    pub attention: Hsla,
    /// Response ready for review (mauve).
    pub ready: Hsla,
    /// Decorative lavender.
    pub lavender: Hsla,
    /// Decorative teal.
    pub teal: Hsla,

    // ── Alpha layers ─────────────────────────────────────────────
    /// Modal backdrop scrim.
    pub backdrop: Hsla,
    /// Text selection highlight.
    pub selection: Hsla,
    /// Diff added-line wash.
    pub diff_add_bg: Hsla,
    /// Diff removed-line wash.
    pub diff_del_bg: Hsla,
}

impl Theme {
    /// The dark (Catppuccin Mocha) theme — currently the only one.
    fn dark() -> Self {
        let c = |v: u32| -> Hsla { rgb(v).into() };
        let ca = |v: u32| -> Hsla { rgba(v).into() };
        Self {
            bg_base: c(0x1e1e2e),
            bg_surface: c(0x181825),
            bg_sunken: c(0x11111b),
            bg_raised: c(0x313244),
            bg_hover: c(0x45475a),
            bg_active: c(0x585b70),
            bg_hover_soft: c(0x2a2a3c),
            bg_row_alt: c(0x1a1a28),
            bg_attention: c(0x2a2334),
            bg_bell: c(0x3a2e3a),

            tint_danger: c(0x3b1f28),
            tint_danger_hover: c(0x58303a),
            tint_danger_soft: c(0x3b1e1e),
            tint_warning: c(0x3b2f1e),
            tint_warning_hover: c(0x4a3f2a),
            tint_warning_soft: c(0x2e2a1e),

            text_primary: c(0xcdd6f4),
            text_body: c(0xbac2de),
            text_secondary: c(0xa6adc8),
            text_muted: c(0x9399b2),
            text_faint: c(0x6c7086),
            text_dim: c(0x585b70),
            text_ghost: c(0x45475a),
            text_on_accent: c(0x1e1e2e),
            text_placeholder: hsla(228.0 / 360.0, 0.17, 0.45, 1.0),

            border_subtle: c(0x313244),
            border_default: c(0x45475a),
            border_strong: c(0x585b70),

            accent: c(0x89b4fa),
            info: c(0x74c7ec),
            success: c(0xa6e3a1),
            warning: c(0xf9e2af),
            danger: c(0xf38ba8),
            danger_soft: c(0xeba0ac),
            attention: c(0xfab387),
            ready: c(0xcba6f7),
            lavender: c(0xb4befe),
            teal: c(0x94e2d5),

            backdrop: ca(0x00000099),
            selection: ca(0x89b4fa55),
            diff_add_bg: ca(0xa6e3a118),
            diff_del_bg: ca(0xf38ba818),
        }
    }
}

/// Global theme accessor. Dark-only today; when a light theme lands this
/// becomes window-appearance aware and callers don't change.
pub fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(Theme::dark)
}

/// A token at reduced opacity — for washes, scrims, and soft borders.
pub fn with_alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}
