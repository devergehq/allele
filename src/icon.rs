//! SVG icon element helper (DEV-4).
//!
//! GPUI's `svg()` paints only when the element itself has a text color —
//! parent hover recoloring does not cascade into it — so icons take an
//! explicit token color and interactive feedback lives on the enclosing
//! button (hover background chips).

use gpui::{px, svg, Hsla, SharedString, Styled, Svg};

/// Icon names, kept in sync with `assets/icons/svg/` and `src/assets.rs`.
pub mod name {
    pub const ALERT_TRIANGLE: &str = "alert-triangle";
    pub const ARCHIVE: &str = "archive";
    pub const CHECK: &str = "check";
    pub const CHEVRON_DOWN: &str = "chevron-down";
    pub const CHEVRON_RIGHT: &str = "chevron-right";
    pub const CIRCLE: &str = "circle";
    pub const CIRCLE_FILL: &str = "circle-fill";
    pub const CLOUD_DOWNLOAD: &str = "cloud-download";
    pub const CLOUD_UPLOAD: &str = "cloud-upload";
    pub const FILE_TEXT: &str = "file-text";
    pub const HELIX: &str = "helix";
    pub const IMAGE: &str = "image";
    pub const LOADER: &str = "loader";
    pub const PAPERCLIP: &str = "paperclip";
    pub const PAUSE: &str = "pause";
    pub const PIN: &str = "pin";
    pub const PLUS: &str = "plus";
    pub const SETTINGS: &str = "settings";
    pub const STAR_FILL: &str = "star-fill";
    pub const TRASH: &str = "trash";
    pub const X: &str = "x";
}

/// A square icon at `size` px tinted `color`.
pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(SharedString::from(format!("icons/svg/{name}.svg")))
        .w(px(size))
        .h(px(size))
        .text_color(color)
        .flex_shrink_0()
}
