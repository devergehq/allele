//! Reload `settings.json` when something other than Allele changes it (DEV-1278).
//!
//! Allele reads the file once at launch and writes its in-memory copy back on
//! any of about forty actions, so an edit made while it ran — by hand, or by an
//! agent following the agent guide — never took effect and was undone at the
//! next save. This module closes that loop:
//!
//! - [`spawn`] watches the file's *directory*, not the file, because editors
//!   save by writing a temp file and renaming it over the old one, and a watch
//!   on the old inode would go quiet after the first save.
//! - [`Settings::reload_from`] re-reads it with the launch loader. Allele's own
//!   saves are recognised by content, not timing: the file holding exactly what
//!   Allele last wrote is a no-op.
//! - [`AppState::apply_settings_reload`] puts the values in place, running the
//!   same handler a Settings-window change would for anything with a side
//!   effect (font size, keep-awake, base infra, remote connections).
//! - `Settings::save_to` refuses a save once the file holds anything but what
//!   Allele last read or wrote, so an in-memory copy older than an outside edit
//!   is never written over it. The outside edit wins.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Context, Window};
use tracing::{info, warn};

use crate::actions::SettingsAction;
use crate::app_state::{AppState, DRAWER_MIN_HEIGHT, RIGHT_SIDEBAR_MIN_WIDTH, SIDEBAR_MIN_WIDTH};
use crate::project::Project;
use crate::settings::{self, Reload, Settings};

/// How long to wait after the first change event before reading the file.
/// An editor's save is several events (write temp, rename, chmod); reading
/// after they settle reads the finished file once instead of a half one.
const SETTLE: Duration = Duration::from_millis(150);

/// Retry delay when the main window is not up yet to apply a reload.
const NOT_READY_RETRY: Duration = Duration::from_millis(500);

/// Watch `path`'s directory and send on `tx` whenever an event names a file
/// called `path`'s file name. Split from [`spawn`] so a test can drive it
/// against a temp directory.
pub(crate) fn watch(
    path: &Path,
    tx: flume::Sender<()>,
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::Watcher;
    let name = path.file_name().map(|n| n.to_os_string());
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if event.paths.iter().any(|p| p.file_name() == name.as_deref()) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Start watching the real settings file. Called once, from
/// [`crate::startup::pollers::spawn_all`].
pub(crate) fn spawn(cx: &mut Context<AppState>) {
    let Some(path) = Settings::path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        // A first run may not have the directory yet; the watch needs it.
        let _ = std::fs::create_dir_all(dir);
    }
    let (tx, rx) = flume::unbounded();
    let watcher = match watch(&path, tx.clone()) {
        Ok(w) => w,
        Err(e) => {
            warn!(
                "Could not watch {} ({e}); edits to it apply after a restart",
                path.display()
            );
            return;
        }
    };
    cx.spawn(async move |this, cx| {
        // Dropping the watcher stops it; it lives as long as this task.
        let _watcher = watcher;
        while rx.recv_async().await.is_ok() {
            cx.background_executor().timer(SETTLE).await;
            rx.drain();

            let read_path = path.clone();
            let reload = cx
                .background_executor()
                .spawn(async move { Settings::reload_from(&read_path) })
                .await;
            if matches!(reload, Reload::Unchanged) {
                continue;
            }
            let applied = crate::dispatch::update_in_main_window(&this, cx, |state, window, cx| {
                state.apply_settings_reload(reload, &path, window, cx)
            });
            if let Err(e) = applied {
                if this.upgrade().is_none() {
                    break; // AppState dropped — app is exiting
                }
                // Not adopted, so the file still reads as changed: try again.
                warn!("settings.json changed but could not be applied yet: {e}");
                cx.background_executor().timer(NOT_READY_RETRY).await;
                let _ = tx.send(());
            }
        }
    })
    .detach();
}

/// What applying a reloaded file to the running projects comes to.
#[derive(Debug, Default)]
pub(crate) struct ProjectChanges {
    /// Index into the running projects, and the file's entry for it. Name and
    /// settings apply; `source_path` applies only to a project with no sessions.
    pub(crate) updates: Vec<(usize, settings::ProjectSave)>,
    /// Projects in the file that are not running; added live.
    pub(crate) added: Vec<settings::ProjectSave>,
    /// Running projects the file no longer lists and that have no sessions.
    pub(crate) removed: Vec<usize>,
    /// Edits not applied, each a log line naming the project and why.
    pub(crate) not_applied: Vec<String>,
}

/// Compare the running projects with the file's list, by project id.
///
/// Name and per-project settings apply live. A new entry is added, as launch
/// adds it. A removed project, or one whose `source_path` changed, applies
/// only when it has no sessions: removing a project closes its sessions and
/// deletes their clones, which a file edit should not be able to do, and its
/// sessions' clones are anchored to the old path. Otherwise the project stays
/// as it is, and the next save writes it back.
pub(crate) fn project_changes(
    running: &[Project],
    file: &[settings::ProjectSave],
) -> ProjectChanges {
    let mut changes = ProjectChanges::default();
    for (idx, project) in running.iter().enumerate() {
        let Some(saved) = file.iter().find(|s| s.id == project.id) else {
            if project.sessions.is_empty() {
                changes.removed.push(idx);
            } else {
                changes.not_applied.push(format!(
                    "project \u{201c}{}\u{201d} was removed from the file but has {} session(s); \
                     it stays — remove it from Allele's sidebar",
                    project.name,
                    project.sessions.len()
                ));
            }
            continue;
        };
        if saved.source_path != project.source_path && !project.sessions.is_empty() {
            changes.not_applied.push(format!(
                "project \u{201c}{}\u{201d}: source_path cannot change while it has sessions; \
                 kept {}",
                project.name,
                project.source_path.display()
            ));
        }
        if saved.name != project.name
            || serde_json::to_value(&saved.settings).ok()
                != serde_json::to_value(&project.settings).ok()
            || (saved.source_path != project.source_path && project.sessions.is_empty())
        {
            changes.updates.push((idx, saved.clone()));
        }
    }
    for saved in file {
        if !running.iter().any(|p| p.id == saved.id) {
            changes.added.push(saved.clone());
        }
    }
    changes
}

/// The Settings-window actions that carry a changed value with a side effect,
/// so a file edit does what the same change in the window does.
pub(crate) fn side_effect_actions(old: &Settings, new: &Settings) -> Vec<SettingsAction> {
    let mut actions = Vec::new();
    if (old.font_size - new.font_size).abs() > f32::EPSILON {
        actions.push(SettingsAction::UpdateFontSize(new.font_size));
    }
    if old.keep_awake_while_working != new.keep_awake_while_working {
        actions.push(SettingsAction::UpdateKeepAwake(
            new.keep_awake_while_working,
        ));
    }
    if old.usage_counting_enabled != new.usage_counting_enabled {
        actions.push(SettingsAction::UpdateUsageCounting(
            new.usage_counting_enabled,
        ));
    }
    if old.browser_integration_enabled != new.browser_integration_enabled {
        actions.push(SettingsAction::UpdateBrowserIntegration(
            new.browser_integration_enabled,
        ));
    }
    if old.base_infra_enabled != new.base_infra_enabled {
        actions.push(SettingsAction::UpdateBaseInfra(new.base_infra_enabled));
    }
    if old.remote.machine_name != new.remote.machine_name {
        actions.push(SettingsAction::UpdateMachineName(
            new.remote.machine_name.clone().unwrap_or_default(),
        ));
    }
    if old.remote.relay_url != new.remote.relay_url {
        actions.push(SettingsAction::UpdateRelayUrl(
            new.remote.relay_url.clone().unwrap_or_default(),
        ));
    }
    if old.remote.allow_connections != new.remote.allow_connections {
        actions.push(SettingsAction::SetAllowRemoteConnections(
            new.remote.allow_connections,
        ));
    }
    actions
}

/// Changed settings that do not reach what is already running, as log lines.
pub(crate) fn later_only(old: &Settings, new: &Settings) -> Vec<String> {
    let mut notes = Vec::new();
    if old.tool_visibility != new.tool_visibility {
        notes.push(
            "tool_visibility applies to sessions opened after this; open ones keep theirs"
                .to_string(),
        );
    }
    notes
}

impl AppState {
    /// Apply what [`Settings::reload_from`] found. Foreground; needs the window
    /// for the font-size broadcast.
    pub(crate) fn apply_settings_reload(
        &mut self,
        reload: Reload,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match reload {
            Reload::Unchanged => {}
            Reload::Missing => info!(
                "settings.json is gone; keeping the running settings, which the next save \
                 writes back"
            ),
            Reload::Unusable(problem) => {
                warn!(
                    "settings.json changed but could not be used ({}); keeping the running \
                     settings",
                    problem.error
                );
                self.sync_notice = Some(problem.summary());
                cx.notify();
            }
            Reload::Changed {
                settings,
                problem,
                bytes,
            } => {
                let notes = self.apply_reloaded_settings(*settings, window, cx);
                Settings::adopt(path, bytes);
                match problem {
                    Some(problem) => self.sync_notice = Some(problem.summary()),
                    None => {
                        let ours = self
                            .sync_notice
                            .as_deref()
                            .is_some_and(|n| n.starts_with(settings::NOTICE_PREFIX));
                        if ours {
                            self.sync_notice = None;
                        }
                    }
                }
                info!("settings.json changed outside Allele; reloaded");
                for note in notes {
                    warn!("settings.json: {note}");
                }
                cx.notify();
            }
        }
    }

    /// Replace the running settings with `new`, read from the file. Returns
    /// log lines for anything that was not applied, or applies only later.
    pub(crate) fn apply_reloaded_settings(
        &mut self,
        new: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        if self.settings_dirty {
            // The decision in the ticket: the outside edit wins.
            warn!(
                "settings.json was edited outside Allele while a change made in Allele was \
                 waiting to be saved; the file wins and the in-Allele change is dropped"
            );
        }
        let old = self.user_settings.clone();
        let mut notes = later_only(&old, &new);

        // Fields with a side effect stay at their old values here, so each
        // handler below sees a change and does what it does in the window.
        self.user_settings = Settings {
            font_size: old.font_size,
            keep_awake_while_working: old.keep_awake_while_working,
            usage_counting_enabled: old.usage_counting_enabled,
            browser_integration_enabled: old.browser_integration_enabled,
            base_infra_enabled: old.base_infra_enabled,
            remote: old.remote.clone(),
            ..new.clone()
        };
        for action in side_effect_actions(&old, &new) {
            let mut skip_refocus = true;
            self.handle_settings_action(action, &mut skip_refocus, window, cx);
        }

        // Layout lives on AppState, not in user_settings.
        self.sidebar.visible = new.sidebar_visible;
        self.sidebar.width = new.sidebar_width.max(SIDEBAR_MIN_WIDTH);
        self.right_panel.visible = new.right_sidebar_visible;
        self.right_panel.width = new.right_sidebar_width.max(RIGHT_SIDEBAR_MIN_WIDTH);
        self.drawer.height = new.drawer_height.max(DRAWER_MIN_HEIGHT);

        let changes = project_changes(&self.projects, &new.projects);
        for (idx, saved) in &changes.updates {
            if let Some(project) = self.projects.get_mut(*idx) {
                project.name = saved.name.clone();
                project.settings = saved.settings.clone();
                if project.sessions.is_empty() {
                    project.source_path = saved.source_path.clone();
                }
            }
        }
        // Highest index first, so the ones still to go keep their index.
        for idx in changes.removed.iter().rev() {
            self.remove_empty_project(*idx);
        }
        for saved in &changes.added {
            let mut project = Project::new(saved.name.clone(), saved.source_path.clone());
            project.id = saved.id.clone();
            project.settings = saved.settings.clone();
            self.projects.push(project);
        }
        notes.extend(changes.not_applied);

        // What is in memory is now what the file says, less anything noted
        // above, so there is nothing to save; the handlers above marked it.
        self.settings_dirty = false;
        if !changes.removed.is_empty() || !changes.added.is_empty() {
            self.mark_state_dirty();
        }
        self.refresh_settings_window(cx);
        notes
    }

    /// Remove a project that has no sessions, fixing up the indices that
    /// point past it. The full [`AppState::remove_project`] also closes
    /// sessions and deletes clones; with no sessions there is none of that.
    fn remove_empty_project(&mut self, idx: usize) {
        if self
            .projects
            .get(idx)
            .is_none_or(|p| !p.sessions.is_empty())
        {
            return;
        }
        self.projects.remove(idx);
        if let Some(active) = self.active.as_mut() {
            if active.project_idx > idx {
                active.project_idx -= 1;
            }
        }
        if let Some(editing) = self.editing_project_settings {
            self.editing_project_settings = match editing.cmp(&idx) {
                std::cmp::Ordering::Less => Some(editing),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(editing - 1),
            };
        }
        self.confirming.dismiss_armed();
    }

    /// Rebuild an open Settings window from the reloaded values, keeping the
    /// section it shows. Its fields mirror `user_settings` and would otherwise
    /// show, and on the next edit push back, the values from before the edit.
    fn refresh_settings_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.settings_window else {
            return;
        };
        // Deferred: rebuilding reads `AppState`, which is mid-update here, and
        // GPUI panics on reading an entity while it is being updated.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |w, _window, cx| w.reload_from_app(cx));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::fixture::{Fixture, FixtureSpec};
    use crate::session::Session;
    use gpui::TestAppContext;

    fn saved(id: &str, name: &str, path: &str) -> settings::ProjectSave {
        settings::ProjectSave {
            id: id.into(),
            name: name.into(),
            source_path: PathBuf::from(path),
            settings: Default::default(),
        }
    }

    fn project(id: &str, name: &str, path: &str) -> Project {
        let mut p = Project::new(name.into(), PathBuf::from(path));
        p.id = id.into();
        p
    }

    fn with_session(mut p: Project) -> Project {
        let now = std::time::SystemTime::now();
        p.sessions.push(Session::suspended_from_persisted(
            "s1".into(),
            "s1".into(),
            now,
            now,
            Duration::ZERO,
            None,
        ));
        p
    }

    #[test]
    fn a_new_project_in_the_file_is_added() {
        let changes = project_changes(&[], &[saved("p1", "alpha", "/tmp/alpha")]);
        assert_eq!(changes.added.len(), 1);
        assert!(changes.not_applied.is_empty());
    }

    #[test]
    fn a_removed_project_with_sessions_stays_and_is_named() {
        let running = [with_session(project("p1", "alpha", "/tmp/alpha"))];
        let changes = project_changes(&running, &[]);
        assert!(changes.removed.is_empty());
        assert_eq!(changes.not_applied.len(), 1);
        assert!(changes.not_applied[0].contains("alpha"));
    }

    #[test]
    fn a_removed_project_without_sessions_goes() {
        let running = [project("p1", "alpha", "/tmp/alpha")];
        assert_eq!(project_changes(&running, &[]).removed, vec![0]);
    }

    #[test]
    fn a_moved_project_with_sessions_keeps_its_path() {
        let running = [with_session(project("p1", "alpha", "/tmp/alpha"))];
        let changes = project_changes(&running, &[saved("p1", "alpha", "/tmp/elsewhere")]);
        assert_eq!(changes.not_applied.len(), 1);
        assert!(changes.updates.is_empty(), "nothing else changed");
    }

    #[test]
    fn a_rename_applies_live() {
        let running = [with_session(project("p1", "alpha", "/tmp/alpha"))];
        let changes = project_changes(&running, &[saved("p1", "renamed", "/tmp/alpha")]);
        assert_eq!(changes.updates.len(), 1);
        assert_eq!(changes.updates[0].1.name, "renamed");
    }

    #[test]
    fn unchanged_settings_need_no_side_effects() {
        let s = Settings::default();
        assert!(side_effect_actions(&s, &s).is_empty());
        let mut bigger = s.clone();
        bigger.font_size = 18.0;
        assert!(matches!(
            side_effect_actions(&s, &bigger).as_slice(),
            [SettingsAction::UpdateFontSize(f)] if *f == 18.0
        ));
    }

    /// The ticket's own case: the dispatch cap, edited in the file, is the cap
    /// the next `allele_sessions_create` is admitted against.
    #[gpui::test]
    fn an_edited_dispatch_cap_applies_without_a_restart(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let mut edited = Settings::default();
        edited.dispatch.max_sessions = 3;
        edited.dispatch.max_depth = 2;
        fx.update(cx, |state, window, cx| {
            state.apply_reloaded_settings(edited, window, cx);
            assert_eq!(state.user_settings.dispatch.max_sessions, 3);
            assert_eq!(state.user_settings.dispatch.max_depth, 2);
            assert!(!state.settings_dirty, "nothing to write back");
        });
    }

    #[gpui::test]
    fn an_unsaved_change_in_allele_loses_to_the_file(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let edited = Settings {
            sidebar_width: 333.0,
            ..Settings::default()
        };
        fx.update(cx, |state, window, cx| {
            state.sidebar.width = 999.0;
            state.mark_settings_dirty();
            state.apply_reloaded_settings(edited, window, cx);
            assert_eq!(state.sidebar.width, 333.0);
            assert!(!state.settings_dirty);
        });
    }

    #[gpui::test]
    fn a_side_effect_runs_its_settings_window_handler(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let mut edited = Settings::default();
        edited.usage_counting_enabled = !edited.usage_counting_enabled;
        edited.font_size = 17.0;
        fx.update(cx, |state, window, cx| {
            state.apply_reloaded_settings(edited.clone(), window, cx);
            assert_eq!(
                state.user_settings.usage_counting_enabled,
                edited.usage_counting_enabled
            );
            assert_eq!(state.user_settings.font_size, 17.0);
            assert!(!state.settings_dirty);
        });
    }

    #[gpui::test]
    fn projects_follow_the_file_unless_they_have_sessions(cx: &mut TestAppContext) {
        let fx = Fixture::build(
            FixtureSpec {
                projects: vec![
                    project("gone", "gone", "/tmp/gone"),
                    with_session(project("busy", "busy", "/tmp/busy")),
                ],
                ..FixtureSpec::default()
            },
            cx,
        );
        let edited = Settings {
            projects: vec![saved("new", "new", "/tmp/new")],
            ..Settings::default()
        };
        fx.update(cx, |state, window, cx| {
            let notes = state.apply_reloaded_settings(edited, window, cx);
            let ids: Vec<&str> = state.projects.iter().map(|p| p.id.as_str()).collect();
            assert_eq!(ids, ["busy", "new"]);
            assert_eq!(notes.len(), 1, "{notes:?}");
        });
    }

    // --- the file side: temp folders only, never the live settings file ------

    fn temp_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("allele-settings-1278-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("settings.json");
        (dir, path)
    }

    fn launch(path: &Path, s: &Settings) {
        s.save_to(path).expect("seed");
        let _ = Settings::load_from(path);
    }

    fn outside_edit(path: &Path, f: impl FnOnce(&mut serde_json::Value)) {
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        f(&mut v);
        // Write-then-rename, as editors save.
        let tmp = path.with_file_name("settings.json.editor~");
        std::fs::write(&tmp, serde_json::to_string(&v).unwrap()).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn changed(reload: Reload) -> (Settings, Option<settings::LoadProblem>, Vec<u8>) {
        match reload {
            Reload::Changed {
                settings,
                problem,
                bytes,
            } => (*settings, problem, bytes),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn allele_s_own_save_is_not_a_reload() {
        let (dir, path) = temp_path("own");
        launch(&path, &Settings::default());
        let s = Settings {
            font_size: 15.0,
            ..Settings::default()
        };
        s.save_to(&path).expect("save");
        assert!(matches!(Settings::reload_from(&path), Reload::Unchanged));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_outside_edit_survives_allele_s_next_save() {
        let (dir, path) = temp_path("survives");
        launch(&path, &Settings::default());
        outside_edit(&path, |v| v["dispatch"]["max_sessions"] = 7.into());

        // A save from the values held before the edit is refused...
        assert!(Settings::default().save_to(&path).is_err());
        // ...the reload brings the edit in...
        let (reloaded, problem, bytes) = changed(Settings::reload_from(&path));
        assert!(problem.is_none());
        assert_eq!(reloaded.dispatch.max_sessions, 7);
        Settings::adopt(&path, bytes);
        // ...and the next save, from the reloaded values, keeps it.
        reloaded.save_to(&path).expect("save after reload");
        let on_disk: Settings =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.dispatch.max_sessions, 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_quick_edits_end_with_the_second() {
        let (dir, path) = temp_path("twice");
        launch(&path, &Settings::default());
        outside_edit(&path, |v| v["dispatch"]["max_sessions"] = 4.into());
        outside_edit(&path, |v| v["dispatch"]["max_sessions"] = 5.into());
        let (reloaded, _, bytes) = changed(Settings::reload_from(&path));
        assert_eq!(reloaded.dispatch.max_sessions, 5);
        Settings::adopt(&path, bytes);
        assert!(matches!(Settings::reload_from(&path), Reload::Unchanged));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bad_value_costs_only_itself_and_is_copied_aside() {
        let (dir, path) = temp_path("bad");
        launch(&path, &Settings::default());
        outside_edit(&path, |v| {
            v["dispatch"]["max_sessions"] = "lots".into();
            v["font_size"] = 19.0.into();
        });
        let (reloaded, problem, _) = changed(Settings::reload_from(&path));
        assert_eq!(reloaded.font_size, 19.0, "the good value applies");
        assert_eq!(
            reloaded.dispatch.max_sessions,
            Settings::default().dispatch.max_sessions,
            "only the bad value is at its default"
        );
        let problem = problem.expect("reported");
        let copy = problem.copy.expect("copied aside");
        assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&path).unwrap());
        assert!(reloaded.save_to(&path).is_err(), "not saved over");

        // Fixing the file lifts the block without a restart.
        outside_edit(&path, |v| v["dispatch"]["max_sessions"] = 6.into());
        let (fixed, problem, bytes) = changed(Settings::reload_from(&path));
        assert!(problem.is_none());
        assert_eq!(fixed.dispatch.max_sessions, 6);
        Settings::adopt(&path, bytes);
        fixed.save_to(&path).expect("saves again once fixed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_half_written_file_keeps_the_running_settings() {
        let (dir, path) = temp_path("half");
        launch(&path, &Settings::default());
        std::fs::write(&path, "{ \"font_size\": 1").unwrap();
        assert!(matches!(Settings::reload_from(&path), Reload::Unusable(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_deleted_file_is_not_a_reload_to_defaults() {
        let (dir, path) = temp_path("deleted");
        launch(&path, &Settings::default());
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(Settings::reload_from(&path), Reload::Missing));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Editors save by renaming a temp file over the old one. A watch on the
    /// old file would go quiet; the directory watch must still fire.
    #[test]
    fn a_write_then_rename_is_seen_by_the_watch() {
        let (dir, path) = temp_path("watch");
        std::fs::write(&path, "{}").unwrap();
        let (tx, rx) = flume::unbounded();
        let _watcher = watch(&path, tx).expect("watch");
        // FSEvents can report changes from just before the watch began.
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}

        let tmp = dir.join("settings.json.tmp-editor");
        std::fs::write(&tmp, "{\"font_size\": 14.0}").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "rename over the file was not seen"
        );

        // And it is still watching after the inode it started on is gone.
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        std::fs::write(&tmp, "{\"font_size\": 15.0}").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "second save was not seen"
        );

        // A file that is not settings.json does not count.
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        std::fs::write(dir.join("keymap.json"), "[]").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
