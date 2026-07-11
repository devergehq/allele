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
                value.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
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
        use objc::{msg_send, sel, sel_impl};

        let app = NSApp();
        let window: id = msg_send![app, keyWindow];
        if window == nil {
            anyhow::bail!("Allele has no key window");
        }
        let view: id = msg_send![window, contentView];
        let bounds: cocoa::foundation::NSRect = msg_send![view, bounds];
        let rep: id = msg_send![view, bitmapImageRepForCachingDisplayInRect: bounds];
        if rep == nil {
            anyhow::bail!("failed to allocate window bitmap");
        }
        let _: () = msg_send![view, cacheDisplayInRect: bounds toBitmapImageRep: rep];
        // NSBitmapImageFileTypePNG = 4.
        let data: id = msg_send![rep, representationUsingType: 4usize properties: nil];
        if data == nil {
            anyhow::bail!("failed to encode window bitmap as PNG");
        }
        let path = NSString::alloc(nil).init_str(&image_path.to_string_lossy());
        let wrote: bool = msg_send![data, writeToFile: path atomically: true];
        let _: () = msg_send![path, release];
        if !wrote {
            anyhow::bail!("failed to write {}", image_path.display());
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
