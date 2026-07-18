use std::path::PathBuf;
use std::process::Command;

use kontur_core::TaskId;

use crate::error::WorkspaceError;
use crate::workspace::{CommandOutput, FrozenDiff, Workspace};

/// Real git effects. The session lives on branch `kontur/<session>` in a
/// dedicated worktree (under the system temp dir), leaving the user's checkout
/// untouched until `merge_session` squash-merges into the original branch.
/// Requires: the target repo's checked-out branch is clean at merge time.
pub struct GitWorkspace {
    repo: PathBuf,
    worktree: PathBuf,
    branch: String,
    /// The branch that was checked out when the worktree was created; the
    /// squash-merge target. Kept for future `base_branch()` accessor.
    #[allow(dead_code)]
    base: String,
}

fn git(dir: &std::path::Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()
        .map_err(|e| WorkspaceError::Io(e.to_string()))?;
    if !out.status.success() {
        return Err(WorkspaceError::Io(format!(
            "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl GitWorkspace {
    pub fn create(repo: PathBuf, session: &str) -> Result<Self, WorkspaceError> {
        let base = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
        let branch = format!("kontur/{session}");
        let mut worktree = std::env::temp_dir();
        worktree.push(format!("kontur-wt-{session}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&worktree);
        git(&repo, &["worktree", "add", worktree.to_str().unwrap(), "-b", &branch])?;
        Ok(GitWorkspace { repo, worktree, branch, base })
    }

    pub fn branch(&self) -> &str { &self.branch }
}

impl Workspace for GitWorkspace {
    fn apply_write(&self, _task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), WorkspaceError> {
        let full = self.worktree.join(path);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        }
        std::fs::write(&full, contents).map_err(|e| WorkspaceError::Io(e.to_string()))
    }

    fn run_command(&self, _task_id: &TaskId, command: &str, cwd: &str) -> Result<CommandOutput, WorkspaceError> {
        let dir = if cwd.is_empty() { self.worktree.clone() } else { self.worktree.join(cwd) };
        let out = Command::new("sh").arg("-c").arg(command).current_dir(&dir).output()
            .map_err(|e| WorkspaceError::Io(e.to_string()))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            exit_code: out.status.code().unwrap_or(-1),
        })
    }

    /// Stage everything, then freeze the staged diff vs HEAD — the exact bytes
    /// the operators review and sign against.
    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError> {
        git(&self.worktree, &["add", "-A"])?;
        let bytes = git(&self.worktree, &["diff", "--cached"])?.into_bytes();
        if bytes.is_empty() {
            return Err(WorkspaceError::UnknownTask(task_id.0.clone()));
        }
        let numstat = git(&self.worktree, &["diff", "--cached", "--numstat"])?;
        let mut files = Vec::new();
        let mut loc = 0u32;
        for line in numstat.lines() {
            let mut parts = line.split_whitespace();
            let adds = parts.next().unwrap_or("0");
            let _dels = parts.next();
            if let Some(name) = parts.next() {
                loc += adds.parse::<u32>().unwrap_or(0);
                files.push(name.to_string());
            }
        }
        Ok(FrozenDiff { bytes, files, loc })
    }

    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        git(&self.worktree, &["add", "-A"])?;
        git(&self.worktree, &["commit", "-m", &format!("kontur: task {}", task_id.0)])?;
        Ok(())
    }

    fn discard_task(&self, _task_id: &TaskId) -> Result<(), WorkspaceError> {
        git(&self.worktree, &["reset", "--hard", "HEAD"])?;
        git(&self.worktree, &["clean", "-fd"])?;
        Ok(())
    }

    fn merge_session(&self, message: &str) -> Result<(), WorkspaceError> {
        git(&self.repo, &["merge", "--squash", &self.branch])?;
        git(&self.repo, &["commit", "-m", message])?;
        git(&self.repo, &["worktree", "remove", "--force", self.worktree.to_str().unwrap()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!("kontur-git-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let run = |args: &[&str]| { git(&p, args).unwrap(); };
        git(&p, &["init", "-b", "main"]).unwrap();
        run(&["config", "user.email", "test@kontur.local"]);
        run(&["config", "user.name", "Kontur Test"]);
        std::fs::write(p.join("README.md"), "seed\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "seed"]);
        p
    }

    #[test]
    fn freeze_accept_and_merge_with_trailers() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo.clone(), "s1").unwrap();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "src/lib.rs", b"pub fn f() {}\n").unwrap();
        let frozen = ws.freeze_task_diff(&t).unwrap();
        assert_eq!(frozen.files, vec!["src/lib.rs".to_string()]);
        assert!(frozen.loc >= 1);
        ws.accept_task(&t).unwrap();
        ws.merge_session("kontur session s1\n\nReviewed-by: A <a>\nReviewed-by: B <b>").unwrap();
        let log = git(&repo, &["log", "-1", "--format=%B", "main"]).unwrap();
        assert!(log.contains("Reviewed-by: A <a>"));
        assert!(log.contains("Reviewed-by: B <b>"));
        let count = git(&repo, &["rev-list", "--count", "main"]).unwrap();
        assert_eq!(count.trim(), "2"); // seed + one squash commit
    }

    #[test]
    fn discard_resets_worktree() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s2").unwrap();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "junk.txt", b"x\n").unwrap();
        let _ = ws.freeze_task_diff(&t).unwrap();
        ws.discard_task(&t).unwrap();
        assert!(ws.freeze_task_diff(&t).is_err()); // nothing left to review
    }

    #[test]
    fn empty_diff_is_an_error() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s3").unwrap();
        assert!(ws.freeze_task_diff(&TaskId("t".into())).is_err());
    }
}
