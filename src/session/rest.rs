//! Rest by loss (DEV-1260): what closing an idle session would lose.
//!
//! Sessions pile up because closing one asks a question the user can't answer
//! cheaply: "is there anything in here I'd miss?" This module answers it. Once
//! a session has been idle for the configured time it gets a [`LossVerdict`],
//! computed by one background git pass and cached until the session's
//! `last_active` changes. The sidebar shows the verdict and fades the row with
//! idle age, and "Put N to rest" archives every session whose verdict is
//! [`LossVerdict::NothingUnique`] through the ordinary archive path.
//!
//! The decisions live here and the row layout lives in `sidebar::render`, so
//! the sidebar can be restructured (DEV-1261) without touching what a verdict
//! means. Nothing in this module runs git on the caller's thread except
//! [`compute`], and that is only ever called from the background executor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::git;
use crate::project::Project;
use crate::session::{Session, SessionStatus};

/// What closing a session would lose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LossVerdict {
    /// The clone is clean, and every commit on it is already on the base
    /// branch by patch identity, so squash-merged work counts as landed.
    NothingUnique,
    /// Work that exists only in this session.
    Loses {
        /// Commits with no patch-equivalent on `base`.
        commits: usize,
        /// Entries the changes panel would list (the same unit as its header).
        changed_files: usize,
        /// The base branch the commits were judged against, by its short name.
        base: String,
    },
    /// The clone path no longer exists on disk. A broken row.
    WorkspaceMissing,
    /// Git could not answer: not a repo, or the base branch doesn't resolve.
    /// Never treated as "nothing", because not knowing is not the same as
    /// having nothing to lose.
    Unknown,
}

impl LossVerdict {
    /// The verdict in words, as the row shows it.
    pub fn label(&self) -> String {
        match self {
            LossVerdict::NothingUnique => "nothing unique".into(),
            LossVerdict::WorkspaceMissing => "workspace missing".into(),
            LossVerdict::Unknown => "can't tell what closing loses".into(),
            LossVerdict::Loses {
                commits,
                changed_files,
                base,
            } => {
                let commits_part = (*commits > 0).then(|| {
                    let noun = if *commits == 1 { "commit" } else { "commits" };
                    format!("{commits} {noun} not on {base}")
                });
                let files_part = (*changed_files > 0).then(|| {
                    let noun = if *changed_files == 1 {
                        "changed file"
                    } else {
                        "changed files"
                    };
                    format!("{changed_files} {noun}")
                });
                commits_part
                    .into_iter()
                    .chain(files_part)
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
        }
    }

    pub fn is_nothing_unique(&self) -> bool {
        matches!(self, LossVerdict::NothingUnique)
    }

    /// True when the session holds work that exists nowhere else.
    pub fn would_lose_work(&self) -> bool {
        matches!(self, LossVerdict::Loses { .. })
    }
}

/// Everything one background verdict pass needs, captured on the foreground
/// so the pass itself never touches `AppState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictJob {
    pub session_id: String,
    /// The `last_active` the verdict is computed for, and cached against.
    pub last_active: SystemTime,
    pub clone_path: PathBuf,
    /// The project's configured base branch (`ProjectSettings::default_branch`).
    pub base: Option<String>,
    pub remote: String,
}

/// Run the git pass for one session. Blocking: call it from the background
/// executor only.
pub fn compute(job: &VerdictJob) -> LossVerdict {
    let clone = &job.clone_path;
    if !clone.exists() {
        return LossVerdict::WorkspaceMissing;
    }
    let Some(changed_files) = git::working_tree_change_count(clone) else {
        return LossVerdict::Unknown;
    };
    let Some((base, base_ref)) = git::resolve_base_ref(clone, job.base.as_deref(), &job.remote)
    else {
        return LossVerdict::Unknown;
    };
    let Some(commits) = git::commits_not_on(clone, &base_ref) else {
        return LossVerdict::Unknown;
    };
    if commits == 0 && changed_files == 0 {
        LossVerdict::NothingUnique
    } else {
        LossVerdict::Loses {
            commits,
            changed_files,
            base,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedVerdict {
    last_active: SystemTime,
    verdict: LossVerdict,
}

/// Verdicts by session id, each valid only for the `last_active` it was
/// computed against. A session that does anything gets a new `last_active`,
/// which makes its entry stale without anyone having to invalidate it.
#[derive(Debug, Default)]
pub struct VerdictCache {
    entries: HashMap<String, CachedVerdict>,
}

impl VerdictCache {
    /// The verdict for this session, if one was computed for this exact
    /// `last_active`.
    pub fn get(&self, session_id: &str, last_active: SystemTime) -> Option<&LossVerdict> {
        self.entries
            .get(session_id)
            .filter(|c| c.last_active == last_active)
            .map(|c| &c.verdict)
    }

    pub fn is_fresh(&self, session_id: &str, last_active: SystemTime) -> bool {
        self.get(session_id, last_active).is_some()
    }

    /// Store a verdict, returning whether what a row would show changed.
    pub fn store(
        &mut self,
        session_id: &str,
        last_active: SystemTime,
        verdict: LossVerdict,
    ) -> bool {
        let changed = self.get(session_id, last_active) != Some(&verdict);
        self.entries.insert(
            session_id.to_string(),
            CachedVerdict {
                last_active,
                verdict,
            },
        );
        changed
    }

    /// Drop entries for sessions that no longer exist.
    pub fn retain_sessions<'a>(&mut self, live: impl IntoIterator<Item = &'a str>) {
        let live: std::collections::HashSet<&str> = live.into_iter().collect();
        self.entries.retain(|id, _| live.contains(id.as_str()));
    }
}

/// How long the session has gone without activity. A clock that moved
/// backwards reads as zero, not as a panic.
pub fn idle_for(last_active: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(last_active).unwrap_or(Duration::ZERO)
}

/// Whether a session has been idle long enough to be judged. A zero threshold
/// turns the feature off.
pub fn is_idle(last_active: SystemTime, now: SystemTime, threshold: Duration) -> bool {
    !threshold.is_zero() && idle_for(last_active, now) >= threshold
}

/// Idle span over which a row fades from [`FADE_START`] to [`FADE_FLOOR`].
pub const FADE_SPAN: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Opacity of a row the moment it crosses the idle threshold.
pub const FADE_START: f32 = 0.85;
/// The dimmest a row gets. Never zero: rows fade, they never disappear.
pub const FADE_FLOOR: f32 = 0.45;

/// Row opacity for an idle age: fully opaque before the threshold, then a
/// linear ramp from [`FADE_START`] down to [`FADE_FLOOR`] over [`FADE_SPAN`].
pub fn fade(idle: Duration, threshold: Duration) -> f32 {
    if threshold.is_zero() || idle < threshold {
        return 1.0;
    }
    let past = (idle - threshold).as_secs_f32() / FADE_SPAN.as_secs_f32();
    FADE_START - (FADE_START - FADE_FLOOR) * past.min(1.0)
}

/// Whether a session is judged at all. It needs its own clone (a session
/// running in the project source has nothing of its own to archive), no
/// agent currently working or waiting on the user, and enough idle time.
fn is_judged(session: &Session, project: &Project, now: SystemTime, threshold: Duration) -> bool {
    let own_clone = session
        .clone_path
        .as_ref()
        .is_some_and(|p| *p != project.source_path);
    let quiet = !matches!(
        session.status,
        SessionStatus::Running | SessionStatus::AwaitingInput
    );
    own_clone && quiet && is_idle(session.last_active, now, threshold)
}

/// The verdict the row for `session` should show, if it is judged and its
/// verdict is current.
pub fn shown_verdict<'a>(
    session: &Session,
    project: &Project,
    cache: &'a VerdictCache,
    now: SystemTime,
    threshold: Duration,
) -> Option<&'a LossVerdict> {
    is_judged(session, project, now, threshold)
        .then(|| cache.get(&session.id, session.last_active))
        .flatten()
}

/// The background passes still owed: judged sessions without a verdict for
/// their current `last_active`. A session whose verdict is cached is never in
/// this list, so a quiet sidebar costs no git at all.
pub fn verdict_jobs(
    projects: &[Project],
    cache: &VerdictCache,
    now: SystemTime,
    threshold: Duration,
) -> Vec<VerdictJob> {
    projects
        .iter()
        .flat_map(|p| p.sessions.iter().map(move |s| (p, s)))
        .filter(|(p, s)| is_judged(s, p, now, threshold))
        .filter(|(_, s)| !cache.is_fresh(&s.id, s.last_active))
        .filter_map(|(p, s)| {
            Some(VerdictJob {
                session_id: s.id.clone(),
                last_active: s.last_active,
                clone_path: s.clone_path.clone()?,
                base: p.settings.default_branch.clone(),
                remote: p.settings.resolved_remote().to_string(),
            })
        })
        .collect()
}

/// Session ids "Put N to rest" archives: every judged session whose current
/// verdict is `nothing unique`, except pinned ones (a pin is the user saying
/// "keep this").
pub fn rest_candidates(
    projects: &[Project],
    cache: &VerdictCache,
    now: SystemTime,
    threshold: Duration,
) -> Vec<String> {
    projects
        .iter()
        .flat_map(|p| p.sessions.iter().map(move |s| (p, s)))
        .filter(|(_, s)| !s.pinned)
        .filter(|(p, s)| {
            shown_verdict(s, p, cache, now, threshold).is_some_and(LossVerdict::is_nothing_unique)
        })
        .map(|(_, s)| s.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=T"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", msg]);
    }

    /// An "origin" work repo on `master`, and a session clone of it sitting
    /// on its own session branch, the shape Allele's clones have.
    struct Fixture {
        _dir: TempDir,
        origin: PathBuf,
        clone: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let origin = dir.path().join("origin");
            std::fs::create_dir(&origin).unwrap();
            git(&origin, &["init", "-q", "-b", "master"]);
            commit_file(&origin, "README", "hello\n", "initial");
            let clone = dir.path().join("clone");
            git(
                dir.path(),
                &["clone", "-q", origin.to_str().unwrap(), "clone"],
            );
            git(&clone, &["checkout", "-q", "-b", "session-abc12345"]);
            Self {
                _dir: dir,
                origin,
                clone,
            }
        }

        fn job(&self, base: Option<&str>) -> VerdictJob {
            VerdictJob {
                session_id: "s".into(),
                last_active: SystemTime::UNIX_EPOCH,
                clone_path: self.clone.clone(),
                base: base.map(str::to_string),
                remote: "origin".into(),
            }
        }

        fn fetch(&self) {
            git(&self.clone, &["fetch", "-q", "origin"]);
        }
    }

    #[test]
    fn clean_clone_with_everything_on_base_is_nothing_unique() {
        let f = Fixture::new();
        assert_eq!(compute(&f.job(None)), LossVerdict::NothingUnique);
        assert_eq!(compute(&f.job(None)).label(), "nothing unique");
    }

    #[test]
    fn work_merged_normally_is_nothing_unique() {
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "work\n", "session work");
        git(
            &f.origin,
            &["pull", "-q", "--no-rebase", "../clone", "session-abc12345"],
        );
        f.fetch();
        assert_eq!(compute(&f.job(None)), LossVerdict::NothingUnique);
    }

    #[test]
    fn squash_merged_single_commit_is_nothing_unique() {
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "work\n", "session work");
        // The same change lands on master as a different commit: another
        // message, another author time — what GitHub's squash merge produces.
        commit_file(&f.origin, "a.txt", "work\n", "Session work (#12)");
        f.fetch();
        // Ancestry alone says it's unpushed; patch identity says it landed.
        let ahead = git(&f.clone, &["rev-list", "--count", "origin/master..HEAD"]);
        assert_eq!(ahead, "1");
        assert_eq!(compute(&f.job(None)), LossVerdict::NothingUnique);
    }

    #[test]
    fn squash_merged_several_commits_is_nothing_unique() {
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "one\n", "first");
        commit_file(&f.clone, "b.txt", "two\n", "second");
        std::fs::write(f.origin.join("a.txt"), "one\n").unwrap();
        commit_file(&f.origin, "b.txt", "two\n", "Squashed (#13)");
        f.fetch();
        assert_eq!(compute(&f.job(None)), LossVerdict::NothingUnique);
    }

    #[test]
    fn one_unpushed_commit_reads_as_one_commit_not_on_base() {
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "work\n", "session work");
        let v = compute(&f.job(None));
        assert_eq!(
            v,
            LossVerdict::Loses {
                commits: 1,
                changed_files: 0,
                base: "master".into()
            }
        );
        assert_eq!(v.label(), "1 commit not on master");
    }

    #[test]
    fn two_dirty_files_read_as_two_changed_files() {
        let f = Fixture::new();
        std::fs::write(f.clone.join("README"), "edited\n").unwrap();
        std::fs::write(f.clone.join("new.txt"), "untracked\n").unwrap();
        let v = compute(&f.job(None));
        assert_eq!(v.label(), "2 changed files");
        assert!(v.would_lose_work());
    }

    #[test]
    fn commits_and_dirty_files_are_both_named() {
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "1\n", "one");
        commit_file(&f.clone, "b.txt", "2\n", "two");
        std::fs::write(f.clone.join("c.txt"), "dirty\n").unwrap();
        assert_eq!(
            compute(&f.job(None)).label(),
            "2 commits not on master · 1 changed file"
        );
    }

    #[test]
    fn missing_clone_reads_as_workspace_missing() {
        let f = Fixture::new();
        let mut job = f.job(None);
        job.clone_path = f.clone.join("gone");
        let v = compute(&job);
        assert_eq!(v, LossVerdict::WorkspaceMissing);
        assert_eq!(v.label(), "workspace missing");
    }

    #[test]
    fn configured_base_branch_is_used_not_guessed() {
        // The DAR-635 shape: work integrates on `develop`, not the default.
        let f = Fixture::new();
        commit_file(&f.clone, "a.txt", "work\n", "session work");
        git(&f.origin, &["checkout", "-q", "-b", "develop"]);
        commit_file(&f.origin, "a.txt", "work\n", "Session work (#14)");
        f.fetch();

        assert_eq!(compute(&f.job(Some("develop"))), LossVerdict::NothingUnique);
        // Written with the remote prefix, the way people type it.
        assert_eq!(
            compute(&f.job(Some("origin/develop"))),
            LossVerdict::NothingUnique
        );
        // Unset, the remote's HEAD (master) is the base, and the work isn't there.
        assert_eq!(compute(&f.job(None)).label(), "1 commit not on master");
    }

    #[test]
    fn configured_base_that_does_not_exist_is_unknown_not_a_guess() {
        let f = Fixture::new();
        assert_eq!(compute(&f.job(Some("trunk"))), LossVerdict::Unknown);
        assert!(!compute(&f.job(Some("trunk"))).is_nothing_unique());
    }

    #[test]
    fn not_a_repo_is_unknown() {
        let dir = TempDir::new().unwrap();
        let job = VerdictJob {
            session_id: "s".into(),
            last_active: SystemTime::UNIX_EPOCH,
            clone_path: dir.path().to_path_buf(),
            base: None,
            remote: "origin".into(),
        };
        assert_eq!(compute(&job), LossVerdict::Unknown);
    }

    // --- cache, idleness, fade ------------------------------------------------

    #[test]
    fn cache_answers_only_for_the_last_active_it_was_computed_for() {
        let mut cache = VerdictCache::default();
        let t0 = SystemTime::UNIX_EPOCH + DAY;
        assert!(cache.store("a", t0, LossVerdict::NothingUnique));
        assert_eq!(cache.get("a", t0), Some(&LossVerdict::NothingUnique));
        assert!(
            !cache.store("a", t0, LossVerdict::NothingUnique),
            "same answer is no change"
        );
        assert_eq!(cache.get("a", t0 + Duration::from_secs(1)), None);
    }

    #[test]
    fn cache_drops_sessions_that_are_gone() {
        let mut cache = VerdictCache::default();
        let t = SystemTime::UNIX_EPOCH;
        cache.store("a", t, LossVerdict::NothingUnique);
        cache.store("b", t, LossVerdict::WorkspaceMissing);
        cache.retain_sessions(["b"]);
        assert!(!cache.is_fresh("a", t));
        assert!(cache.is_fresh("b", t));
    }

    #[test]
    fn idleness_needs_the_threshold_and_zero_disables_it() {
        let now = SystemTime::UNIX_EPOCH + 10 * DAY;
        assert!(is_idle(now - 2 * DAY, now, DAY));
        assert!(!is_idle(now - Duration::from_secs(60), now, DAY));
        assert!(!is_idle(now - 9 * DAY, now, Duration::ZERO));
        // A last_active in the future (clock skew) is not idle.
        assert!(!is_idle(now + DAY, now, DAY));
    }

    #[test]
    fn rows_fade_with_idle_age_but_never_vanish() {
        assert_eq!(fade(Duration::from_secs(60), DAY), 1.0);
        assert_eq!(fade(DAY, DAY), FADE_START);
        let a_week = fade(8 * DAY, DAY);
        let a_month = fade(31 * DAY, DAY);
        assert!(a_week < FADE_START && a_week > a_month);
        assert_eq!(a_month, FADE_FLOOR);
        assert_eq!(fade(400 * DAY, DAY), FADE_FLOOR);
        const { assert!(FADE_FLOOR > 0.0) };
        assert_eq!(fade(400 * DAY, Duration::ZERO), 1.0);
    }

    // --- selection over projects ------------------------------------------------

    fn session(id: &str, last_active: SystemTime, clone: Option<&str>) -> Session {
        Session::suspended_from_persisted(
            id.into(),
            id.into(),
            last_active,
            last_active,
            Duration::ZERO,
            clone.map(PathBuf::from),
        )
    }

    fn project_with(sessions: Vec<Session>) -> Project {
        let mut p = Project::new("p".into(), PathBuf::from("/src/p"));
        p.settings.default_branch = Some("develop".into());
        p.sessions = sessions;
        p
    }

    #[test]
    fn verdict_is_not_recomputed_while_last_active_is_unchanged() {
        let now = SystemTime::UNIX_EPOCH + 100 * DAY;
        let idle_since = now - 3 * DAY;
        let mut projects = vec![project_with(vec![session("a", idle_since, Some("/ws/a"))])];
        let mut cache = VerdictCache::default();

        let jobs = verdict_jobs(&projects, &cache, now, DAY);
        assert_eq!(jobs.len(), 1, "an idle, unjudged session is owed a pass");
        assert_eq!(
            jobs[0].base.as_deref(),
            Some("develop"),
            "the project's base"
        );
        assert_eq!(jobs[0].last_active, idle_since);
        cache.store("a", jobs[0].last_active, LossVerdict::NothingUnique);

        // Later ticks, same last_active: nothing to do, however often we ask.
        for later in [now + Duration::from_secs(15), now + 5 * DAY] {
            assert!(verdict_jobs(&projects, &cache, later, DAY).is_empty());
        }

        // The session did something: the cached verdict no longer applies,
        // and once it is idle again it is owed a fresh pass.
        let touched = now + DAY;
        projects[0].sessions[0].last_active = touched;
        assert_eq!(
            shown_verdict(&projects[0].sessions[0], &projects[0], &cache, touched, DAY),
            None
        );
        assert!(
            verdict_jobs(&projects, &cache, touched, DAY).is_empty(),
            "not idle yet"
        );
        let jobs = verdict_jobs(&projects, &cache, touched + 2 * DAY, DAY);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].last_active, touched);
    }

    #[test]
    fn sessions_that_are_not_judged_get_no_pass() {
        let now = SystemTime::UNIX_EPOCH + 100 * DAY;
        let old = now - 30 * DAY;
        let mut running = session("running", old, Some("/ws/r"));
        running.status = SessionStatus::Running;
        let mut waiting = session("waiting", old, Some("/ws/w"));
        waiting.status = SessionStatus::AwaitingInput;
        let projects = vec![project_with(vec![
            running,
            waiting,
            session("fresh", now - Duration::from_secs(60), Some("/ws/f")),
            session("no-clone", old, None),
            // Fallback mode: the "clone" is the project source itself.
            session("in-source", old, Some("/src/p")),
        ])];
        let cache = VerdictCache::default();
        assert!(verdict_jobs(&projects, &cache, now, DAY).is_empty());
    }

    #[test]
    fn put_to_rest_selects_exactly_the_nothing_unique_sessions() {
        let now = SystemTime::UNIX_EPOCH + 100 * DAY;
        let old = now - 10 * DAY;
        let mut pinned = session("pinned", old, Some("/ws/pinned"));
        pinned.pinned = true;
        let mut projects = vec![
            project_with(vec![
                session("clean-1", old, Some("/ws/c1")),
                session("lossy", old, Some("/ws/l")),
                session("missing", old, Some("/ws/m")),
                session("unknown", old, Some("/ws/u")),
                pinned,
                session("not-yet-judged", old, Some("/ws/n")),
            ]),
            project_with(vec![session("clean-2", old, Some("/ws/c2"))]),
        ];
        let mut cache = VerdictCache::default();
        for id in ["clean-1", "clean-2", "pinned"] {
            cache.store(id, old, LossVerdict::NothingUnique);
        }
        cache.store(
            "lossy",
            old,
            LossVerdict::Loses {
                commits: 1,
                changed_files: 0,
                base: "develop".into(),
            },
        );
        cache.store("missing", old, LossVerdict::WorkspaceMissing);
        cache.store("unknown", old, LossVerdict::Unknown);

        assert_eq!(
            rest_candidates(&projects, &cache, now, DAY),
            vec!["clean-1".to_string(), "clean-2".to_string()]
        );

        // A verdict from before the session's latest activity is not trusted.
        projects[0].sessions[0].last_active = now - 2 * DAY;
        assert_eq!(
            rest_candidates(&projects, &cache, now, DAY),
            vec!["clean-2".to_string()]
        );
    }

    #[test]
    fn rested_sessions_are_restorable_from_their_archive_refs() {
        // Put to rest goes through `remove_session`, whose git half is
        // `archive_session` into canonical. Run that half for two
        // nothing-unique sessions and restore each from its ref.
        let f = Fixture::new();
        let canonical = &f.origin;
        let dir = TempDir::new().unwrap();
        let mut heads = Vec::new();
        for id in ["11111111-rest", "22222222-rest"] {
            let clone = dir.path().join(id);
            git(
                dir.path(),
                &["clone", "-q", canonical.to_str().unwrap(), id],
            );
            git(
                &clone,
                &["checkout", "-q", "-b", &git::session_branch_name(id)],
            );
            let job = VerdictJob {
                session_id: id.into(),
                last_active: SystemTime::UNIX_EPOCH,
                clone_path: clone.clone(),
                base: None,
                remote: "origin".into(),
            };
            assert_eq!(compute(&job), LossVerdict::NothingUnique);
            heads.push(git(&clone, &["rev-parse", "HEAD"]));
            git::archive_session(canonical, &clone, id).unwrap();
        }
        for (id, head) in ["11111111-rest", "22222222-rest"].iter().zip(heads) {
            let archived = git(canonical, &["rev-parse", &git::archive_ref_name(id)]);
            assert_eq!(archived, head);
            let restored = format!("restored-{}", &id[..8]);
            git::restore_archive_branch(canonical, id, &restored).unwrap();
            assert_eq!(git(canonical, &["rev-parse", "HEAD"]), head);
            git(canonical, &["checkout", "-q", "master"]);
        }
    }
}
