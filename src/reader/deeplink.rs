//! Cross-surface deep-link protocol (DEV-44).
//!
//! A `DeepLink` names an exact location — a file line, a transcript view, a
//! diff hunk, a PR, or an external URL — independent of what is on screen.
//! [`AppState::navigate`] restores the project, session, surface, and location
//! a link points at, falling back to external tools when no in-app surface can
//! host it. Links round-trip through an `allele://` URL so notifications and
//! other processes can hand one back.

use std::path::PathBuf;

use gpui::{Context, Window};

use crate::actions::SessionCursor;
use crate::app_state::{AppState, MainTab};

/// A resolved navigation target.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeepLink {
    /// A file, optionally at a line, optionally in a specific project/session.
    File {
        project: Option<String>,
        session: Option<String>,
        path: PathBuf,
        line: Option<usize>,
    },
    /// The transcript surface for a project/session.
    Transcript {
        project: Option<String>,
        session: Option<String>,
    },
    /// A diff hunk for a repo-relative path at a line.
    Diff { path: PathBuf, line: Option<usize> },
    /// A pull request (chapter routing lands with the review surface).
    Pr { number: u64 },
    /// A plain external URL — opened with the platform handler.
    External(String),
}

impl DeepLink {
    /// Parse an `allele://…` URL, an `http(s)://…` URL, or a bare `path:line`.
    pub(crate) fn parse(input: &str) -> Option<DeepLink> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        if s.starts_with("http://") || s.starts_with("https://") {
            return Some(DeepLink::External(s.to_string()));
        }
        if let Some(rest) = s.strip_prefix("allele://") {
            let (kind, query) = match rest.split_once('?') {
                Some((k, q)) => (k, q),
                None => (rest, ""),
            };
            let params = parse_query(query);
            let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            return match kind {
                "file" => Some(DeepLink::File {
                    project: get("project"),
                    session: get("session"),
                    path: PathBuf::from(get("path")?),
                    line: get("line").and_then(|l| l.parse().ok()),
                }),
                "transcript" => Some(DeepLink::Transcript {
                    project: get("project"),
                    session: get("session"),
                }),
                "diff" => Some(DeepLink::Diff {
                    path: PathBuf::from(get("path")?),
                    line: get("line").and_then(|l| l.parse().ok()),
                }),
                "pr" => Some(DeepLink::Pr {
                    number: get("number")?.parse().ok()?,
                }),
                _ => None,
            };
        }
        // Bare `path:line` (or just `path`).
        if let Some((path, line)) = s.rsplit_once(':') {
            if let Ok(n) = line.parse::<usize>() {
                return Some(DeepLink::File {
                    project: None,
                    session: None,
                    path: PathBuf::from(path),
                    line: Some(n),
                });
            }
        }
        Some(DeepLink::File {
            project: None,
            session: None,
            path: PathBuf::from(s),
            line: None,
        })
    }

    /// Serialize back to an `allele://` URL (external URLs pass through).
    pub(crate) fn to_url(&self) -> String {
        match self {
            DeepLink::External(u) => u.clone(),
            DeepLink::File {
                project,
                session,
                path,
                line,
            } => {
                let mut q = vec![("path".to_string(), path.to_string_lossy().into_owned())];
                if let Some(l) = line {
                    q.push(("line".to_string(), l.to_string()));
                }
                if let Some(p) = project {
                    q.push(("project".to_string(), p.clone()));
                }
                if let Some(s) = session {
                    q.push(("session".to_string(), s.clone()));
                }
                format!("allele://file?{}", build_query(&q))
            }
            DeepLink::Transcript { project, session } => {
                let mut q = Vec::new();
                if let Some(p) = project {
                    q.push(("project".to_string(), p.clone()));
                }
                if let Some(s) = session {
                    q.push(("session".to_string(), s.clone()));
                }
                format!("allele://transcript?{}", build_query(&q))
            }
            DeepLink::Diff { path, line } => {
                let mut q = vec![("path".to_string(), path.to_string_lossy().into_owned())];
                if let Some(l) = line {
                    q.push(("line".to_string(), l.to_string()));
                }
                format!("allele://diff?{}", build_query(&q))
            }
            DeepLink::Pr { number } => format!("allele://pr?number={number}"),
        }
    }
}

impl AppState {
    /// Parse a link string (`allele://…`, an external URL, or `path:line`) and
    /// route it. This is the entry point notifications and other surfaces use —
    /// they hand over a serialized URL rather than a typed value.
    pub(crate) fn open_deep_link(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(link) = DeepLink::parse(url) {
            self.navigate(link, window, cx);
        }
    }

    /// Route a deep link: restore project/session/surface/location, or fall
    /// back to an external tool.
    pub(crate) fn navigate(
        &mut self,
        link: DeepLink,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match link {
            DeepLink::External(url) => {
                self.platform.shell.open_url(&url);
            }
            DeepLink::File {
                project,
                session,
                path,
                line,
            } => {
                self.restore_context(&project, &session);
                // Resolve relative paths against the workspace root.
                let abs = if path.is_absolute() {
                    path
                } else {
                    match self.reader_workspace_root() {
                        Some(root) => root.join(&path),
                        None => path,
                    }
                };
                if abs.exists() {
                    self.reveal_file(abs, line, cx);
                } else {
                    // No in-app target — fall back to the external editor.
                    self.open_in_external_editor(&abs);
                }
            }
            DeepLink::Transcript { project, session } => {
                self.restore_context(&project, &session);
                self.main_tab = MainTab::Transcript;
                cx.notify();
            }
            DeepLink::Diff { path, line } => {
                // Route to the review surface: reveal the changed file in the
                // Reader at the hunk line (the diff panel selection follows the
                // changes list, which already tracks the active session).
                let abs = match self.reader_workspace_root() {
                    Some(root) => root.join(&path),
                    None => path,
                };
                self.right_panel.visible = true;
                if abs.exists() {
                    self.reveal_file(abs, line, cx);
                }
                cx.notify();
            }
            DeepLink::Pr { number: _ } => {
                // PR-chapter routing arrives with the review surface; for now
                // open the changes panel as the closest in-app surface.
                self.right_panel.visible = true;
                cx.notify();
            }
        }
    }

    /// Switch to the file in the Reader and mark a line to reveal.
    pub(crate) fn reveal_file(
        &mut self,
        path: PathBuf,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        // Expand tree ancestors so the row is visible.
        if let Some(root) = self.reader_workspace_root() {
            let mut cur = path.parent().map(|p| p.to_path_buf());
            while let Some(dir) = cur {
                if dir.starts_with(&root) && dir != root {
                    self.reader.expanded_dirs.insert(dir.clone());
                    cur = dir.parent().map(|p| p.to_path_buf());
                } else {
                    break;
                }
            }
        }
        self.reader.selected_path = Some(path.clone());
        self.main_tab = MainTab::Reader;
        self.load_preview(path);
        // load_preview resets reveal_line; set it after so the source view
        // scrolls to and highlights the target.
        self.reader.reveal_line = line;
        cx.notify();
    }

    /// Resolve and activate a project/session by name, when both are given.
    /// Missing names leave the current selection in place.
    fn restore_context(&mut self, project: &Option<String>, session: &Option<String>) {
        let project_idx = match project {
            Some(name) => self.projects.iter().position(|p| &p.name == name),
            None => self.active.map(|c| c.project_idx),
        };
        let Some(p_idx) = project_idx else {
            return;
        };
        let session_idx = match session {
            Some(label) => self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.iter().position(|s| &s.label == label)),
            None => self
                .active
                .filter(|c| c.project_idx == p_idx)
                .map(|c| c.session_idx)
                .or(Some(0)),
        };
        if let Some(s_idx) = session_idx {
            if self
                .projects
                .get(p_idx)
                .map(|p| s_idx < p.sessions.len())
                .unwrap_or(false)
            {
                self.active = Some(SessionCursor {
                    project_idx: p_idx,
                    session_idx: s_idx,
                });
            }
        }
    }
}

/// Parse `a=1&b=2` into decoded pairs.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn build_query(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Minimal percent-encoding of the reserved set we actually emit.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'%' | b'&' | b'=' | b'?' | b'#' | b' ' => out.push_str(&format!("%{b:02X}")),
            _ => out.push(b as char),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_link_round_trips() {
        let link = DeepLink::File {
            project: Some("Allele".into()),
            session: Some("feature x".into()),
            path: PathBuf::from("src/app_state.rs"),
            line: Some(42),
        };
        let url = link.to_url();
        assert_eq!(DeepLink::parse(&url), Some(link));
    }

    #[test]
    fn bare_path_and_line() {
        assert_eq!(
            DeepLink::parse("src/main.rs:120"),
            Some(DeepLink::File {
                project: None,
                session: None,
                path: PathBuf::from("src/main.rs"),
                line: Some(120),
            })
        );
    }

    #[test]
    fn external_url_passthrough() {
        let l = DeepLink::parse("https://example.com/x").unwrap();
        assert_eq!(l, DeepLink::External("https://example.com/x".into()));
        assert_eq!(l.to_url(), "https://example.com/x");
    }
}
