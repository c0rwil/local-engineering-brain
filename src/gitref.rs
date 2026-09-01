//! Shared-truth checkouts: index `origin/<default>` without touching anyone's
//! working tree.
//!
//! Documentation surfaces (the workspace browser, vault exports) should
//! reflect what the TEAM has merged, not whatever branch or dirty state a
//! local checkout happens to be on. This module fetches the remote default
//! branch and materializes it as a throwaway detached `git worktree` under
//! the LEB state directory; the caller indexes that path (identity resolves
//! to the same project via the canonical remote) and the worktree is removed
//! on drop. The developer's checkout is never written to. Agent-facing
//! context compilation intentionally keeps using the local tree — that is
//! what the agent is editing.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// A detached worktree at the remote default branch; removed on drop.
pub struct OriginWorktree {
    repo: PathBuf,
    path: PathBuf,
    /// The ref that was checked out (e.g. `origin/main`).
    pub reference: String,
}

impl OriginWorktree {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OriginWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| Error::Git(format!("git {}: {error}", args.join(" "))))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The remote default branch ref (`origin/main`, `origin/master`, …).
///
/// # Errors
///
/// Returns an error when the repo has no usable `origin` default branch.
pub fn origin_default_ref(repo: &Path) -> Result<String> {
    if let Ok(head) = git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = head.strip_prefix("refs/remotes/") {
            return Ok(name.to_owned());
        }
    }
    for candidate in ["origin/main", "origin/master"] {
        if git(repo, &["rev-parse", "--verify", "--quiet", candidate]).is_ok() {
            return Ok(candidate.to_owned());
        }
    }
    Err(Error::Git(
        "no origin/main or origin/master found; is `origin` configured?".to_owned(),
    ))
}

/// Fetch the remote default branch and materialize it as a throwaway
/// detached worktree under `state_dir`. Fetch failures (offline) degrade to
/// the last-known remote-tracking ref rather than failing.
///
/// # Errors
///
/// Returns an error when no origin default exists or the worktree cannot be
/// created.
pub fn checkout_origin_default(repo: &Path, state_dir: &Path) -> Result<OriginWorktree> {
    // Fetch is best-effort: offline documentation refreshes fall back to the
    // last-fetched remote state, which is still shared truth (just older).
    let _ = git(repo, &["fetch", "--quiet", "origin"]);
    let reference = origin_default_ref(repo)?;
    let commit = git(repo, &["rev-parse", &reference])?;
    let short = &commit[..commit.len().min(12)];
    let path = state_dir.join(format!("wt-{short}-{}", std::process::id()));
    std::fs::create_dir_all(state_dir)
        .map_err(|error| Error::Git(format!("worktree parent: {error}")))?;
    git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            "--force",
            &path.to_string_lossy(),
            &commit,
        ],
    )?;
    Ok(OriginWorktree {
        repo: repo.to_path_buf(),
        path,
        reference,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn origin_worktree_reflects_the_remote_not_the_local_branch() {
        let temp = TempDir::new().expect("temp");
        let origin = temp.path().join("origin.git");
        let seed = temp.path().join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        run(&seed, &["init", "-q", "-b", "main"]);
        run(&seed, &["config", "user.email", "leb@example.invalid"]);
        run(&seed, &["config", "user.name", "LEB Tests"]);
        std::fs::write(seed.join("lib.rs"), "pub fn one() {}\n").expect("file");
        run(&seed, &["add", "."]);
        run(&seed, &["commit", "-qm", "one"]);
        run(
            temp.path(),
            &["clone", "-q", "--bare", "seed", "origin.git"],
        );

        // Developer clone: local branch diverges with UNMERGED work.
        let dev = temp.path().join("dev");
        run(
            temp.path(),
            &["clone", "-q", &origin.to_string_lossy(), "dev"],
        );
        run(&dev, &["config", "user.email", "leb@example.invalid"]);
        run(&dev, &["config", "user.name", "LEB Tests"]);
        std::fs::write(dev.join("lib.rs"), "pub fn local_experiment() {}\n").expect("file");
        run(&dev, &["commit", "-aqm", "local only"]);

        // Meanwhile main moved on the remote.
        std::fs::write(seed.join("lib.rs"), "pub fn merged_truth() {}\n").expect("file");
        run(&seed, &["commit", "-aqm", "merged"]);
        run(&seed, &["push", "-q", &origin.to_string_lossy(), "main"]);

        let state = temp.path().join("state");
        let worktree = checkout_origin_default(&dev, &state).expect("worktree");
        assert_eq!(worktree.reference, "origin/main");
        let content =
            std::fs::read_to_string(worktree.path().join("lib.rs")).expect("worktree file");
        assert!(
            content.contains("merged_truth"),
            "worktree must show the remote main, got: {content}"
        );
        assert!(
            std::fs::read_to_string(dev.join("lib.rs"))
                .expect("dev file")
                .contains("local_experiment"),
            "the developer checkout must be untouched"
        );
        let path = worktree.path().to_path_buf();
        drop(worktree);
        assert!(!path.exists(), "worktree cleaned up on drop");
    }
}
