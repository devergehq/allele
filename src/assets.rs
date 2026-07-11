//! Embedded asset source for GPUI (DEV-4).
//!
//! Serves the bundled SVG icon set to `gpui::svg()` elements. Assets are
//! compiled into the binary with `include_bytes!` so the .app bundle needs
//! no loose resource files and paths can't break at runtime.

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

/// One entry per bundled asset. Paths are what render code passes to
/// `svg().path(...)` — keep them in sync with `src/icon.rs`.
macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        fn lookup(path: &str) -> Option<&'static [u8]> {
            match path {
                $($path => Some(include_bytes!(concat!("../assets/", $path)).as_slice()),)*
                _ => None,
            }
        }
        fn all_paths() -> &'static [&'static str] {
            &[$($path),*]
        }
    };
}

assets!(
    "icons/svg/alert-triangle.svg",
    "icons/svg/archive.svg",
    "icons/svg/check.svg",
    "icons/svg/chevron-down.svg",
    "icons/svg/chevron-right.svg",
    "icons/svg/circle-fill.svg",
    "icons/svg/circle.svg",
    "icons/svg/file-text.svg",
    "icons/svg/helix.svg",
    "icons/svg/image.svg",
    "icons/svg/loader.svg",
    "icons/svg/paperclip.svg",
    "icons/svg/pause.svg",
    "icons/svg/pin.svg",
    "icons/svg/plus.svg",
    "icons/svg/settings.svg",
    "icons/svg/star-fill.svg",
    "icons/svg/trash.svg",
    "icons/svg/x.svg",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(lookup(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(all_paths()
            .iter()
            .filter(|p| p.starts_with(path))
            .map(|p| SharedString::from(*p))
            .collect())
    }
}
