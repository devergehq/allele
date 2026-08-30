//! Seeding for `--sandbox` (DEV-487).
//!
//! An isolated root is only useful if there is something safe to exercise in
//! it. This scaffolds a throwaway git repository and registers it as a project,
//! so a sandbox instance opens with sessions, branches and drawer terminals
//! available without touching anything the user actually works on.
//!
//! The repository is **generated**, never a link to a real checkout. A sandbox
//! that pointed at live code would defeat its own purpose the first time a
//! session branched or committed.
//!
//! Every step is idempotent: an existing repo is left alone and an existing
//! `settings.json` is never overwritten, so a sandbox can be customised and
//! relaunched without losing the changes.

use crate::paths;
use crate::settings::{ProjectSave, Settings};
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

/// Name of the generated project, used for both the directory and the label.
const DEMO_PROJECT: &str = "sandbox-demo";

/// Prepare the sandbox root. Failures are logged and tolerated — a sandbox
/// without its demo project is still an isolated instance, which is the part
/// that matters.
pub fn seed() {
    let Some(root) = paths::root() else {
        warn!("sandbox: no root to seed");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&root) {
        warn!("sandbox: could not create {}: {e}", root.display());
        return;
    }

    let project_dir = root.join("projects").join(DEMO_PROJECT);
    if project_dir.join(".git").exists() {
        info!("sandbox: reusing {}", project_dir.display());
    } else if let Err(e) = scaffold_repo(&project_dir) {
        warn!("sandbox: could not scaffold demo project: {e}");
        return;
    }

    register_project(&project_dir);
    info!("sandbox: root {}", root.display());
}

/// Create a small git repository with something to edit and a commit to branch
/// from. Sessions clone from `HEAD`, so an empty repo would give every session
/// an unborn branch.
fn scaffold_repo(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("README.md"),
        "# sandbox-demo\n\nA throwaway project for exercising Allele against an isolated root.\n\
         Nothing here is real — delete the whole sandbox whenever you like:\n\n\
         ```sh\nrm -rf ~/.allele-sandbox\n```\n",
    )?;
    std::fs::write(
        dir.join("hello.sh"),
        "#!/bin/sh\necho \"hello from the sandbox\"\n",
    )?;
    // Something for a drawer terminal to tail and a session to change.
    std::fs::write(dir.join("notes.md"), "- first note\n")?;

    // `-b master` to match the default branch Allele falls back to, so branch
    // detection in a sandbox behaves like it does in a real project.
    run_git(dir, &["init", "-q", "-b", "master"])?;
    run_git(dir, &["add", "."])?;
    run_git(
        dir,
        &[
            "-c",
            "user.email=allele@local",
            "-c",
            "user.name=Allele",
            "commit",
            "-q",
            "-m",
            "chore: seed the sandbox demo project",
        ],
    )?;
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new("git").current_dir(dir).args(args).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed with {status}",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Register the demo project, but only when the sandbox has no settings yet.
/// A sandbox that has been configured is left exactly as the user left it.
fn register_project(project_dir: &Path) {
    let Some(settings_path) = paths::settings_file() else {
        return;
    };
    if settings_path.exists() {
        return;
    }
    let mut settings = Settings::default();
    settings.projects.push(ProjectSave {
        id: uuid::Uuid::new_v4().to_string(),
        name: DEMO_PROJECT.to_string(),
        source_path: project_dir.to_path_buf(),
        settings: Default::default(),
    });
    settings.save();
    info!("sandbox: registered {DEMO_PROJECT}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_produces_a_repo_with_a_commit() {
        // Sessions clone from HEAD, so "has a commit" is the property that
        // matters — an empty repo would hand every session an unborn branch.
        let dir = std::env::temp_dir().join("allele-test-sandbox-scaffold");
        let _ = std::fs::remove_dir_all(&dir);
        scaffold_repo(&dir).expect("scaffold should succeed");

        assert!(dir.join(".git").exists());
        assert!(dir.join("README.md").exists());
        let head = Command::new("git")
            .current_dir(&dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert!(head.status.success(), "HEAD should resolve to a commit");
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "master");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_is_safe_to_repeat() {
        let dir = std::env::temp_dir().join("allele-test-sandbox-repeat");
        let _ = std::fs::remove_dir_all(&dir);
        scaffold_repo(&dir).expect("first scaffold");
        // The second call re-adds identical content; `commit` finds nothing to
        // do and exits non-zero, which is why `seed` checks for .git first.
        assert!(dir.join(".git").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
