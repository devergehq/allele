//! Incremental, resilient workspace file index (DEV-40).
//!
//! The file list backing Cmd+P and content search is built off the UI thread
//! and cached, so large repositories stay responsive. A generation counter
//! discards stale results when the workspace root changes mid-build, and the
//! status is surfaced so failures are visible rather than silent.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::Context;

use crate::app_state::AppState;

/// Where the index is in its lifecycle.
#[derive(Clone, PartialEq)]
pub(crate) enum IndexStatus {
    /// Never built for the current root.
    Idle,
    /// A background build is in flight.
    Building,
    /// `files` reflects the current root.
    Ready,
    /// The last build failed; the string explains why.
    Failed(String),
}

/// Cached list of workspace files plus its build state.
pub(crate) struct FileIndex {
    /// Root the current `files` were collected from.
    pub(crate) root: Option<PathBuf>,
    /// Absolute file paths. Shared so consumers (the palette, search) clone
    /// the `Arc`, not the vector.
    pub(crate) files: Arc<Vec<PathBuf>>,
    pub(crate) status: IndexStatus,
    /// Bumped on every (re)build kickoff; async results carrying an older
    /// generation are dropped.
    pub(crate) generation: u64,
}

impl Default for FileIndex {
    fn default() -> Self {
        FileIndex {
            root: None,
            files: Arc::new(Vec::new()),
            status: IndexStatus::Idle,
            generation: 0,
        }
    }
}

impl AppState {
    /// Build the index if the workspace root changed or it was never built.
    /// Cheap no-op when the cache is already fresh.
    pub(crate) fn ensure_file_index(&mut self, cx: &mut Context<Self>) {
        let root = self.reader_workspace_root();
        if root != self.file_index.root {
            self.refresh_file_index(cx);
        } else if self.file_index.status == IndexStatus::Idle {
            self.refresh_file_index(cx);
        }
    }

    /// Rebuild the index for the current workspace root on the background
    /// executor. Mirrors the changes-panel refresh: bump a generation, spawn,
    /// then apply only if still current.
    pub(crate) fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.reader_workspace_root() else {
            self.file_index = FileIndex::default();
            cx.notify();
            return;
        };

        self.file_index.generation += 1;
        let generation = self.file_index.generation;
        self.file_index.root = Some(root.clone());
        self.file_index.status = IndexStatus::Building;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::reader::palette::collect_files_result(&root) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.file_index.generation != generation {
                    return; // superseded by a newer build
                }
                match result {
                    Ok(files) => {
                        this.file_index.files = Arc::new(files);
                        this.file_index.status = IndexStatus::Ready;
                    }
                    Err(e) => {
                        tracing::warn!("file index build failed: {e}");
                        this.file_index.status = IndexStatus::Failed(e);
                    }
                }
                // If the palette is open against this root, re-point it at the
                // fresh list and rescore the current query.
                if let Some(palette) = this.file_palette.as_mut() {
                    if this.file_index.root.as_deref() == Some(palette.root.as_path()) {
                        palette.files = this.file_index.files.clone();
                        let q = this.file_palette_input.read(cx).text().to_string();
                        palette.recompute(&q);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
