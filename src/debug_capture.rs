//! Agent-facing UI capture bridge.
//!
//! `Allele --capture-ui` writes a request under `~/.allele/debug/`. The
//! running app consumes it and snapshots its own NSView, which does not need
//! macOS Screen Recording permission.

use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REQUEST_FILE: &str = "capture.request";
const IMAGE_FILE: &str = "latest.png";
const METADATA_FILE: &str = "latest.json";

#[derive(Serialize)]
pub(crate) struct CaptureMetadata<'a> {
    pub status: &'a str,
    pub timestamp_ms: u128,
    pub image_path: String,
    pub width: f64,
    pub height: f64,
    pub active_project: Option<&'a str>,
    pub active_session: Option<&'a str>,
    pub main_tab: &'a str,
    pub sidebar_visible: bool,
    pub changes_visible: bool,
    pub drawer_visible: bool,
    pub error: Option<String>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn debug_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".allele").join("debug"))
}

pub(crate) fn take_request() -> bool {
    let Some(path) = debug_dir().map(|dir| dir.join(REQUEST_FILE)) else {
        return false;
    };
    path.exists() && std::fs::remove_file(path).is_ok()
}

/// Agent CLI entry point. Returns after a fresh result appears or times out.
pub(crate) fn request_capture_and_wait() -> anyhow::Result<PathBuf> {
    let dir = debug_dir().ok_or_else(|| anyhow::anyhow!("home directory unavailable"))?;
    std::fs::create_dir_all(&dir)?;
    let started = SystemTime::now();
    std::fs::write(dir.join(REQUEST_FILE), format!("{}\n", now_ms()))?;

    let metadata = dir.join(METADATA_FILE);
    for _ in 0..50 {
        if metadata
            .metadata()
            .and_then(|m| m.modified())
            .map(|modified| modified >= started)
            .unwrap_or(false)
        {
            let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&metadata)?)?;
            if value.get("status").and_then(|v| v.as_str()) == Some("ok") {
                return Ok(dir.join(IMAGE_FILE));
            }
            anyhow::bail!(
                "capture failed: {}",
                value
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for running Allele app")
}

pub(crate) fn capture(metadata: CaptureMetadata<'_>) -> anyhow::Result<()> {
    let dir = debug_dir().ok_or_else(|| anyhow::anyhow!("home directory unavailable"))?;
    std::fs::create_dir_all(&dir)?;
    let image_path = dir.join(IMAGE_FILE);

    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::appkit::NSApp;
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};
        use std::ffi::c_void;

        type CGWindowID = u32;
        type CGWindowListOption = u32;
        type CGWindowImageOption = u32;

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            static CGRectNull: cocoa::foundation::NSRect;
            fn CGWindowListCreateImage(
                screen_bounds: cocoa::foundation::NSRect,
                list_option: CGWindowListOption,
                window_id: CGWindowID,
                image_option: CGWindowImageOption,
            ) -> *mut c_void;
            fn CGImageRelease(image: *mut c_void);
            fn CGImageGetWidth(image: *mut c_void) -> usize;
            fn CGImageGetHeight(image: *mut c_void) -> usize;
        }

        // Capture only our key window. Unlike NSView bitmap caching, the
        // window server includes GPUI's CAMetalLayer in the resulting image.
        const OPTION_INCLUDING_WINDOW: CGWindowListOption = 1 << 3;
        const IMAGE_BOUNDS_IGNORE_FRAMING: CGWindowImageOption = 1 << 0;
        const IMAGE_BEST_RESOLUTION: CGWindowImageOption = 1 << 3;

        let app = NSApp();
        let window: id = msg_send![app, keyWindow];
        if window == nil {
            anyhow::bail!("Allele has no key window");
        }
        let window_id: CGWindowID = msg_send![window, windowNumber];
        let image = CGWindowListCreateImage(
            CGRectNull,
            OPTION_INCLUDING_WINDOW,
            window_id,
            IMAGE_BOUNDS_IGNORE_FRAMING | IMAGE_BEST_RESOLUTION,
        );
        if image.is_null() {
            anyhow::bail!("window server returned no image for window {window_id}");
        }

        let rep: id = msg_send![class!(NSBitmapImageRep), alloc];
        let rep: id = msg_send![rep, initWithCGImage: image];
        let width = CGImageGetWidth(image);
        let height = CGImageGetHeight(image);
        CGImageRelease(image);
        if rep == nil {
            anyhow::bail!("failed to create bitmap from window image");
        }
        // NSBitmapImageFileTypePNG = 4.
        let data: id = msg_send![rep, representationUsingType: 4usize properties: nil];
        if data == nil {
            let _: () = msg_send![rep, release];
            anyhow::bail!("failed to encode window bitmap as PNG");
        }
        let path = NSString::alloc(nil).init_str(&image_path.to_string_lossy());
        let wrote: bool = msg_send![data, writeToFile: path atomically: true];
        let _: () = msg_send![path, release];
        let _: () = msg_send![rep, release];
        if !wrote {
            anyhow::bail!("failed to write {}", image_path.display());
        }
        if width == 0 || height == 0 {
            anyhow::bail!("window server returned an empty image");
        }
    }

    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("UI capture is currently supported only on macOS");

    let json = serde_json::to_vec_pretty(&metadata)?;
    let temp = dir.join("latest.json.tmp");
    std::fs::write(&temp, json)?;
    std::fs::rename(temp, dir.join(METADATA_FILE))?;
    Ok(())
}

pub(crate) fn write_error(mut metadata: CaptureMetadata<'_>, error: anyhow::Error) {
    let Some(dir) = debug_dir() else { return };
    metadata.status = "error";
    metadata.error = Some(error.to_string());
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_vec_pretty(&metadata) {
            let _ = std::fs::write(dir.join(METADATA_FILE), json);
        }
    }
}
