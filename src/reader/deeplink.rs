//! Cross-surface deep-link protocol (DEV-44).
//!
//! A `DeepLink` names an exact location — a file line, a transcript view, a
//! diff hunk, a PR, or an external URL — independent of what is on screen.
//! [`AppState::navigate`] restores the project, session, surface, and location
//! a link points at, falling back to external tools when no in-app surface can
//! host it. Links round-trip through an `allele://` URL so notifications and
//! other processes can hand one back.
//!
//! Links carry a [`LinkOrigin`] (DEV-1252). A link the human typed or picked
//! goes anywhere, as it always has. A link an agent wrote — rich-mode
//! assistant text today, session faces later — may only open files inside the
//! clone of the session that wrote it, checked against the canonical path at
//! click time so a symlink planted after the text was written cannot escape.

use std::path::{Path, PathBuf};

use gpui::Context;

use crate::actions::SessionCursor;
use crate::app_state::{AppState, MainTab};

/// Who put a link on screen, which decides how far it may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkOrigin {
    /// Typed, searched for, or otherwise chosen by the human. Unconfined.
    Human,
    /// Written by the agent running in the session with this Allele session
    /// id. File and diff targets are confined to that session's clone.
    Agent { session: String },
}

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
        // Any other `scheme://` — `file://`, `vscode://`, a custom handler — is
        // inert. Without this it fell through to the bare-path branch below and
        // was handed to the external editor as a "file" (DEV-1252).
        if has_foreign_scheme(s) {
            return None;
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
    pub(crate) fn open_deep_link(&mut self, url: &str, origin: LinkOrigin, cx: &mut Context<Self>) {
        if let Some(link) = DeepLink::parse(url) {
            self.navigate(link, origin, cx);
        }
    }

    /// Route a deep link: restore project/session/surface/location, or fall
    /// back to an external tool.
    ///
    /// An [`LinkOrigin::Agent`] link is confined: its `project`/`session`
    /// parameters are ignored (the agent does not get to pick whose clone it
    /// is judged against), and a file or diff target outside the originating
    /// session's clone is refused with the reason shown in the notice banner.
    pub(crate) fn navigate(&mut self, link: DeepLink, origin: LinkOrigin, cx: &mut Context<Self>) {
        if let LinkOrigin::Agent { session } = &origin {
            self.navigate_agent(link, session, cx);
            return;
        }
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

    /// The agent-origin half of [`Self::navigate`].
    fn navigate_agent(&mut self, link: DeepLink, session_id: &str, cx: &mut Context<Self>) {
        let (path, line, is_diff) = match link {
            DeepLink::File { path, line, .. } => (path, line, false),
            DeepLink::Diff { path, line } => (path, line, true),
            // External URLs open as they would for the human; the renderer
            // shows the full URL on hover so the text cannot disguise it.
            DeepLink::External(url) => {
                self.platform.shell.open_url(&url);
                return;
            }
            // Neither reaches the file system. Their project/session params
            // are still ignored: the originating session is the context.
            DeepLink::Transcript { .. } => {
                if self.activate_session_by_id(session_id) {
                    self.main_tab = MainTab::Transcript;
                }
                cx.notify();
                return;
            }
            DeepLink::Pr { .. } => {
                self.right_panel.visible = true;
                cx.notify();
                return;
            }
        };
        let Some(root) = self.session_workspace_root(session_id) else {
            self.refuse_link(
                &path,
                "the session that wrote it is no longer open".to_string(),
                cx,
            );
            return;
        };
        match confine_to_workspace(&root, &path) {
            Ok(abs) => {
                self.activate_session_by_id(session_id);
                if is_diff {
                    self.right_panel.visible = true;
                }
                self.reveal_file(abs, line, cx);
            }
            Err(refusal) => self.refuse_link(&path, refusal.reason(), cx),
        }
    }

    /// Show why an agent link did not open. Non-modal: the dismissible notice
    /// banner, never a dialog, and never a silent drop.
    fn refuse_link(&mut self, path: &Path, reason: String, cx: &mut Context<Self>) {
        tracing::warn!("refused agent link to {}: {reason}", path.display());
        self.sync_notice = Some(format!(
            "Didn\u{2019}t open \u{201c}{}\u{201d}: {reason}.",
            path.display()
        ));
        cx.notify();
    }

    /// The clone (or, before provisioning, the source tree) of the session with
    /// this Allele id — the same root the Reader uses for an active session.
    fn session_workspace_root(&self, session_id: &str) -> Option<PathBuf> {
        self.projects.iter().find_map(|p| {
            p.sessions.iter().find(|s| s.id == session_id).map(|s| {
                s.clone_path
                    .clone()
                    .unwrap_or_else(|| p.source_path.clone())
            })
        })
    }

    /// Make the session with this Allele id active. False if it is gone.
    fn activate_session_by_id(&mut self, session_id: &str) -> bool {
        for (project_idx, p) in self.projects.iter().enumerate() {
            if let Some(session_idx) = p.sessions.iter().position(|s| s.id == session_id) {
                self.active = Some(SessionCursor {
                    project_idx,
                    session_idx,
                });
                return true;
            }
        }
        false
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

/// Why an agent link was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkRefusal {
    /// The target resolves (after symlinks) outside the session's clone.
    OutsideWorkspace,
    /// The target does not exist, so it cannot be shown to be inside.
    Missing,
}

impl LinkRefusal {
    pub(crate) fn reason(&self) -> String {
        match self {
            LinkRefusal::OutsideWorkspace => "it is outside this session\u{2019}s workspace",
            LinkRefusal::Missing => "it does not exist in this session\u{2019}s workspace",
        }
        .to_string()
    }
}

/// Resolve an agent-written path against the session clone `root` and require
/// the result to lie inside it.
///
/// Both sides are canonicalised, so `..` components and symlinks are resolved
/// by the file system rather than guessed at lexically: a link to an in-clone
/// symlink that points at `~/.ssh` resolves to `~/.ssh/…` and is refused.
/// This runs at click time, so a symlink planted after the text was written is
/// caught too. A target that does not exist is refused rather than handed to
/// the external editor — nothing that cannot be canonicalised is trusted.
pub(crate) fn confine_to_workspace(root: &Path, path: &Path) -> Result<PathBuf, LinkRefusal> {
    let root = root
        .canonicalize()
        .map_err(|_| LinkRefusal::OutsideWorkspace)?;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let target = joined.canonicalize().map_err(|_| LinkRefusal::Missing)?;
    if target.starts_with(&root) {
        Ok(target)
    } else {
        Err(LinkRefusal::OutsideWorkspace)
    }
}

/// True for `scheme://…` with any scheme other than the allow-listed ones the
/// caller has already handled. A Windows-style `C:\` or a bare `path:line`
/// has no `//` after the colon and is not matched.
fn has_foreign_scheme(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else {
        return false;
    };
    rest.starts_with("//")
        && !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
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

#[cfg(test)]
mod confinement_tests {
    //! DEV-1252: agent-origin links are confined to the session's clone.
    use super::*;
    use crate::app_state::fixture::Fixture;
    use crate::app_state::MainTab;
    use crate::project::Project;
    use crate::session::Session;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    /// A temp dir holding `clone/` (with `src/main.rs`) and a sibling
    /// `outside` file the agent must not reach.
    fn layout() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        fs::create_dir_all(clone.join("src")).unwrap();
        fs::write(clone.join("src/main.rs"), "fn main() {}\n".repeat(20)).unwrap();
        fs::write(tmp.path().join("outside"), "secret").unwrap();
        (tmp, clone)
    }

    #[test]
    fn an_absolute_path_outside_the_clone_is_refused() {
        let (_tmp, clone) = layout();
        assert_eq!(
            confine_to_workspace(&clone, Path::new("/etc/hosts")),
            Err(LinkRefusal::OutsideWorkspace)
        );
    }

    #[test]
    fn dot_dot_out_of_the_clone_is_refused() {
        let (_tmp, clone) = layout();
        assert_eq!(
            confine_to_workspace(&clone, Path::new("../outside")),
            Err(LinkRefusal::OutsideWorkspace)
        );
        // A `..` that stays inside is fine.
        assert!(confine_to_workspace(&clone, Path::new("src/../src/main.rs")).is_ok());
    }

    #[test]
    fn a_symlink_out_of_the_clone_is_refused_when_clicked() {
        let (tmp, clone) = layout();
        let link = clone.join("notes.md");
        // When the agent wrote the link the symlink pointed inside…
        symlink(clone.join("src/main.rs"), &link).unwrap();
        assert!(confine_to_workspace(&clone, Path::new("notes.md")).is_ok());
        // …then it was swapped to point outside. The click re-resolves it.
        fs::remove_file(&link).unwrap();
        symlink(tmp.path().join("outside"), &link).unwrap();
        assert_eq!(
            confine_to_workspace(&clone, Path::new("notes.md")),
            Err(LinkRefusal::OutsideWorkspace)
        );
    }

    #[test]
    fn a_path_and_line_inside_the_clone_resolves() {
        let (_tmp, clone) = layout();
        let Some(DeepLink::File { path, line, .. }) = DeepLink::parse("src/main.rs:10") else {
            panic!("expected a file link");
        };
        assert_eq!(line, Some(10));
        assert_eq!(
            confine_to_workspace(&clone, &path),
            Ok(clone.canonicalize().unwrap().join("src/main.rs"))
        );
    }

    #[test]
    fn a_missing_target_is_refused_not_handed_to_the_editor() {
        let (_tmp, clone) = layout();
        assert_eq!(
            confine_to_workspace(&clone, Path::new("src/nope.rs")),
            Err(LinkRefusal::Missing)
        );
    }

    #[test]
    fn file_and_vscode_schemes_are_inert() {
        for url in [
            "file:///etc/hosts",
            "file:///Users/me/.ssh/id_ed25519:1",
            "vscode://file/etc/hosts",
            "vscode://file/etc/hosts:10",
            "x-custom+handler://anything",
        ] {
            assert_eq!(DeepLink::parse(url), None, "{url} must not parse");
        }
        // The allow-list itself is unchanged.
        assert!(matches!(
            DeepLink::parse("https://example.com"),
            Some(DeepLink::External(_))
        ));
        assert!(matches!(
            DeepLink::parse("allele://file?path=src/main.rs"),
            Some(DeepLink::File { .. })
        ));
        assert!(matches!(
            DeepLink::parse("src/main.rs:3"),
            Some(DeepLink::File { .. })
        ));
    }

    /// Two sessions, `s-1` and `s-2`, each with its own clone; `s-1` active.
    fn fixture(cx: &mut gpui::TestAppContext) -> (Fixture, tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, clone1) = layout();
        let clone2 = tmp.path().join("clone2");
        fs::create_dir_all(&clone2).unwrap();
        fs::write(clone2.join("other.rs"), "x").unwrap();
        let mut project = Project::new("alpha".into(), tmp.path().to_path_buf());
        let now = SystemTime::now();
        for (id, clone) in [("s-1", &clone1), ("s-2", &clone2)] {
            project.sessions.push(Session::suspended_from_persisted(
                id.into(),
                id.into(),
                now,
                now,
                Duration::ZERO,
                Some(clone.clone()),
            ));
        }
        let fx = Fixture::build(
            crate::app_state::fixture::FixtureSpec {
                projects: vec![project],
                active: Some(SessionCursor {
                    project_idx: 0,
                    session_idx: 0,
                }),
                ..Default::default()
            },
            cx,
        );
        (fx, tmp, clone1, clone2)
    }

    fn agent() -> LinkOrigin {
        LinkOrigin::Agent {
            session: "s-1".into(),
        }
    }

    #[gpui::test]
    fn an_agent_link_to_etc_hosts_is_refused_with_the_reason_shown(cx: &mut gpui::TestAppContext) {
        let (fx, _tmp, _c1, _c2) = fixture(cx);
        let (selected, notice) = fx.update(cx, |state, _w, cx| {
            state.open_deep_link("/etc/hosts", agent(), cx);
            (
                state.reader.selected_path.clone(),
                state.sync_notice.clone(),
            )
        });
        assert_eq!(selected, None, "nothing outside the clone is revealed");
        let notice = notice.expect("a refusal is shown, not dropped");
        assert!(notice.contains("/etc/hosts"), "{notice}");
        assert!(notice.contains("outside this session"), "{notice}");
    }

    #[gpui::test]
    fn an_agent_link_inside_the_clone_opens_at_its_line(cx: &mut gpui::TestAppContext) {
        let (fx, _tmp, clone1, _c2) = fixture(cx);
        let (selected, line, on_reader, notice) = fx.update(cx, |state, _w, cx| {
            state.open_deep_link("src/main.rs:10", agent(), cx);
            (
                state.reader.selected_path.clone(),
                state.reader.reveal_line,
                matches!(state.main_tab, MainTab::Reader),
                state.sync_notice.clone(),
            )
        });
        assert_eq!(
            selected,
            Some(clone1.canonicalize().unwrap().join("src/main.rs"))
        );
        assert_eq!(line, Some(10));
        assert!(on_reader);
        assert_eq!(notice, None);
    }

    #[gpui::test]
    fn an_agent_cannot_borrow_another_sessions_clone(cx: &mut gpui::TestAppContext) {
        let (fx, _tmp, _c1, clone2) = fixture(cx);
        let url = DeepLink::File {
            project: Some("alpha".into()),
            session: Some("s-2".into()),
            path: clone2.join("other.rs"),
            line: None,
        }
        .to_url();
        let (selected, notice) = fx.update(cx, |state, _w, cx| {
            state.open_deep_link(&url, agent(), cx);
            (
                state.reader.selected_path.clone(),
                state.sync_notice.clone(),
            )
        });
        assert_eq!(selected, None);
        assert!(notice.is_some());
    }

    #[gpui::test]
    fn an_agent_diff_link_out_of_the_clone_is_refused(cx: &mut gpui::TestAppContext) {
        let (fx, _tmp, _c1, _c2) = fixture(cx);
        let (selected, notice) = fx.update(cx, |state, _w, cx| {
            state.open_deep_link("allele://diff?path=../outside", agent(), cx);
            (
                state.reader.selected_path.clone(),
                state.sync_notice.clone(),
            )
        });
        assert_eq!(selected, None);
        assert!(notice.is_some());
    }

    #[gpui::test]
    fn a_human_link_to_an_absolute_path_is_unconfined(cx: &mut gpui::TestAppContext) {
        let (fx, tmp, _c1, _c2) = fixture(cx);
        let outside = tmp.path().join("outside");
        let (selected, notice) = fx.update(cx, |state, _w, cx| {
            state.open_deep_link(&outside.to_string_lossy(), LinkOrigin::Human, cx);
            (
                state.reader.selected_path.clone(),
                state.sync_notice.clone(),
            )
        });
        assert_eq!(selected, Some(outside), "revealed exactly as before");
        assert_eq!(notice, None);
    }
}
