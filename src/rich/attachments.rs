//! Attachment pipeline for Rich Mode's compose bar.
//!
//! Files (or pasted images) selected via picker, drag-drop, or clipboard paste
//! are copied into `~/.allele/attachments/<session_id>/<uuid>.<ext>`. The
//! original filename is retained in metadata for the UI; Claude receives the
//! on-disk path via the submit preamble and reads it with its Read tool.
//!
//! Three lifecycle entry points:
//!   - `copy_file` / `save_image` — attach during compose
//!   - `cleanup_session` — called from `remove_session` on explicit discard
//!   - `sweep_orphans` — startup sweep to catch force-quit leftovers
//!
//! All copy/save calls create the session directory on demand. No state beyond
//! the filesystem.

use gpui::{Image, ImageFormat};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// One attached file tracked by the compose bar.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: Uuid,
    pub original_name: String,
    pub path: PathBuf,
    pub is_image: bool,
}

impl Attachment {
    /// Short label for the compose-bar card (≤20 chars, ellipsised).
    pub fn display_label(&self) -> String {
        let n = &self.original_name;
        if n.chars().count() <= 20 {
            n.clone()
        } else {
            let prefix: String = n.chars().take(19).collect();
            format!("{prefix}…")
        }
    }

    /// Heuristic: Claude's Read tool handles text, images, and PDFs.
    /// Anything else (executables, archives, compiled formats) gets a ⚠
    /// on the card so the user knows Claude won't see the content.
    pub fn is_binary_unreadable(&self) -> bool {
        let ext = self
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        !READABLE_EXTS.iter().any(|e| *e == ext)
    }
}

/// Extensions Claude's Read tool can ingest (text, images, PDF).
const READABLE_EXTS: &[&str] = &[
    // Text
    "txt", "md", "markdown", "json", "yaml", "yml", "toml", "xml", "html",
    "css", "scss", "js", "ts", "tsx", "jsx", "py", "rb", "rs", "go", "c",
    "cpp", "h", "hpp", "cc", "java", "kt", "swift", "sh", "bash", "zsh",
    "fish", "ps1", "bat", "ini", "cfg", "conf", "log", "csv", "tsv", "sql",
    "env", "properties", "gitignore", "dockerfile",
    // Image (multimodal)
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tiff", "tif",
    // Structured doc
    "pdf",
];

/// Root directory for all attachments (shared across sessions as subdirs).
///
/// `~/.allele/attachments/`. Returns `None` if the user has no home dir
/// (in which case attachments are disabled — not fatal).
pub fn attachments_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".allele").join("attachments"))
}

/// Per-session attachments directory.
pub fn attachments_dir(session_id: &str) -> Option<PathBuf> {
    attachments_root().map(|r| r.join(session_id))
}

fn ensure_session_dir(session_id: &str) -> std::io::Result<PathBuf> {
    let dir = attachments_dir(session_id).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory")
    })?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Copy a file on disk into the session's attachment dir.
///
/// Returns an `Attachment` describing the new on-disk location. The source
/// is not modified or moved — it's a copy.
pub fn copy_file(src: &Path, session_id: &str) -> std::io::Result<Attachment> {
    let dir = ensure_session_dir(session_id)?;
    let id = Uuid::new_v4();
    let ext = sanitise_ext(src.extension().and_then(|e| e.to_str()).unwrap_or(""));
    let filename = if ext.is_empty() {
        id.to_string()
    } else {
        format!("{id}.{ext}")
    };
    let dst = dir.join(&filename);
    fs::copy(src, &dst)?;
    let original_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| filename.clone());
    let is_image = IMAGE_EXTS.iter().any(|e| *e == ext.as_str());
    Ok(Attachment {
        id,
        original_name,
        path: dst,
        is_image,
    })
}

/// Save an in-memory clipboard image as an attachment on disk.
pub fn save_image(image: &Image, session_id: &str) -> std::io::Result<Attachment> {
    let dir = ensure_session_dir(session_id)?;
    let id = Uuid::new_v4();
    let ext = image_format_ext(image.format);
    let filename = format!("{id}.{ext}");
    let dst = dir.join(&filename);
    fs::write(&dst, &image.bytes)?;
    Ok(Attachment {
        id,
        original_name: format!("pasted-image.{ext}"),
        path: dst,
        is_image: true,
    })
}

/// Recursively remove the session's attachment dir. Best-effort — missing
/// directories are not errors.
pub fn cleanup_session(session_id: &str) {
    if let Some(dir) = attachments_dir(session_id) {
        if dir.exists() {
            if let Err(e) = fs::remove_dir_all(&dir) {
                eprintln!(
                    "allele: attachments cleanup failed for session {session_id}: {e}"
                );
            }
        }
    }
}

/// Remove any per-session attachment dir whose session_id is not in
/// `active_session_ids`. Catches force-quit leftovers.
pub fn sweep_orphans(active_session_ids: &[String]) {
    let Some(root) = attachments_root() else { return; };
    let Ok(entries) = fs::read_dir(&root) else { return; };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if active_session_ids.iter().any(|id| id == name) {
            continue;
        }
        if let Err(e) = fs::remove_dir_all(&path) {
            eprintln!(
                "allele: orphan attachment sweep failed for {}: {e}",
                path.display()
            );
        }
    }
}

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tiff", "tif",
];

fn image_format_ext(fmt: ImageFormat) -> &'static str {
    match fmt {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Gif => "gif",
        ImageFormat::Webp => "webp",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Ico => "ico",
    }
}

/// Strip an extension to alphanumeric-only (lowercased). Guards against
/// pathological source filenames contributing path separators or
/// nul bytes to our target path.
fn sanitise_ext(ext: &str) -> String {
    ext.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_ext_strips_non_alphanumeric() {
        assert_eq!(sanitise_ext("png"), "png");
        assert_eq!(sanitise_ext("JPG"), "jpg");
        assert_eq!(sanitise_ext("../etc"), "etc");
        assert_eq!(sanitise_ext(""), "");
        assert_eq!(sanitise_ext("ta/r"), "tar");
    }

    #[test]
    fn display_label_truncates() {
        let a = Attachment {
            id: Uuid::new_v4(),
            original_name: "short.txt".into(),
            path: PathBuf::new(),
            is_image: false,
        };
        assert_eq!(a.display_label(), "short.txt");

        let b = Attachment {
            id: Uuid::new_v4(),
            original_name: "this-is-a-very-long-filename-that-should-be-truncated.png".into(),
            path: PathBuf::new(),
            is_image: true,
        };
        assert_eq!(b.display_label().chars().count(), 20);
        assert!(b.display_label().ends_with('…'));
    }

    #[test]
    fn binary_unreadable_detection() {
        let text = Attachment {
            id: Uuid::new_v4(),
            original_name: "code.rs".into(),
            path: PathBuf::from("code.rs"),
            is_image: false,
        };
        assert!(!text.is_binary_unreadable());

        let binary = Attachment {
            id: Uuid::new_v4(),
            original_name: "program.exe".into(),
            path: PathBuf::from("program.exe"),
            is_image: false,
        };
        assert!(binary.is_binary_unreadable());
    }

    // Combined into a single test because both touch the shared
    // `~/.allele/attachments/` root; running them concurrently (cargo's
    // default) races each other's directories.
    #[test]
    fn filesystem_pipeline_roundtrip_and_sweep() {
        // Part 1 — copy_file + cleanup_session round-trip.
        let tmp_src = std::env::temp_dir()
            .join(format!("allele-attach-test-{}.txt", Uuid::new_v4()));
        fs::write(&tmp_src, b"hello world").unwrap();

        let session_a = format!("test-a-{}", Uuid::new_v4());
        let attachment = copy_file(&tmp_src, &session_a).expect("copy_file");
        assert!(attachment.path.exists());
        assert_eq!(attachment.original_name, tmp_src.file_name().unwrap().to_str().unwrap());
        assert_eq!(fs::read_to_string(&attachment.path).unwrap(), "hello world");

        cleanup_session(&session_a);
        assert!(!attachment.path.exists());
        assert!(!attachments_dir(&session_a).unwrap().exists());
        let _ = fs::remove_file(&tmp_src);

        // Part 2 — sweep_orphans removes dirs not in the active set.
        let Some(root) = attachments_root() else { return; };
        let orphan_id = format!("orphan-{}", Uuid::new_v4());
        let orphan_dir = root.join(&orphan_id);
        fs::create_dir_all(&orphan_dir).unwrap();
        fs::write(orphan_dir.join("marker.txt"), b"stale").unwrap();

        let known_id = format!("known-{}", Uuid::new_v4());
        let known_dir = root.join(&known_id);
        fs::create_dir_all(&known_dir).unwrap();

        // Pass both the known id and ALL other top-level dirs as active so
        // this test only actually sweeps our own `orphan-<uuid>`. Otherwise
        // any concurrent test or real user session dir would get wiped out.
        let mut active = vec![known_id.clone()];
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name != orphan_id {
                        active.push(name.to_string());
                    }
                }
            }
        }
        sweep_orphans(&active);
        assert!(!orphan_dir.exists(), "orphan should be removed");
        assert!(known_dir.exists(), "known session dir should survive");

        let _ = fs::remove_dir_all(&known_dir);
    }
}
