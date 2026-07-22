use std::path::PathBuf;
use std::process::Command;

use kontur_core::TaskId;

use crate::error::WorkspaceError;
use crate::workspace::{contained_join, CommandOutput, FrozenDiff, Workspace};

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

pub(crate) fn git(dir: &std::path::Path, args: &[&str]) -> Result<String, WorkspaceError> {
    // Kontur-issued commits are mechanical (task commits, squash-merge); the
    // signed, tamper-evident record is the audit chain, not git signatures.
    // Disabling gpg here keeps sessions immune to gpg-agent failures under a
    // user's global commit.gpgsign=true.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .map_err(|e| WorkspaceError::Io(e.to_string()))?;
    if !out.status.success() {
        return Err(WorkspaceError::Io(format!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl GitWorkspace {
    pub fn create(repo: PathBuf, session: &str) -> Result<Self, WorkspaceError> {
        let base = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string();

        // Pre-flight: refuse to start a session on a dirty checkout so the
        // squash-merge at the end cannot conflict with uncommitted work.
        let status = git(&repo, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorkspaceError::Io(
                "repository checkout is dirty; commit or stash before hosting a session".into(),
            ));
        }

        let branch = format!("kontur/{session}");
        let mut worktree = std::env::temp_dir();
        worktree.push(format!("kontur-wt-{session}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&worktree);
        let worktree_str = worktree
            .to_str()
            .ok_or_else(|| WorkspaceError::Io("worktree path is not valid UTF-8".into()))?;
        git(&repo, &["worktree", "add", worktree_str, "-b", &branch])?;
        Ok(GitWorkspace {
            repo,
            worktree,
            branch,
            base,
        })
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }
}

impl Workspace for GitWorkspace {
    fn apply_write(
        &self,
        _task_id: &TaskId,
        path: &str,
        contents: &[u8],
    ) -> Result<(), WorkspaceError> {
        let full = contained_join(&self.worktree, path)?;
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        }
        std::fs::write(&full, contents).map_err(|e| WorkspaceError::Io(e.to_string()))
    }

    fn run_command(
        &self,
        _task_id: &TaskId,
        command: &str,
        cwd: &str,
    ) -> Result<CommandOutput, WorkspaceError> {
        let dir = if cwd.is_empty() {
            self.worktree.clone()
        } else {
            self.worktree.join(cwd)
        };
        let out = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&dir)
            .output()
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
                // loc = added lines only (numstat additions), not net change
                loc += adds.parse::<u32>().unwrap_or(0);
                files.push(name.to_string());
            }
        }
        Ok(FrozenDiff { bytes, files, loc })
    }

    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        git(&self.worktree, &["add", "-A"])?;
        git(
            &self.worktree,
            &["commit", "-m", &format!("kontur: task {}", task_id.0)],
        )?;
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
        let worktree_str = self
            .worktree
            .to_str()
            .ok_or_else(|| WorkspaceError::Io("worktree path is not valid UTF-8".into()))?;
        git(&self.repo, &["worktree", "remove", "--force", worktree_str])?;
        Ok(())
    }

    fn audit_dir(&self) -> Option<PathBuf> {
        Some(self.repo.join(".kontur"))
    }

    fn read_file(&self, _task_id: &TaskId, path: &str) -> Result<Option<Vec<u8>>, WorkspaceError> {
        let full = contained_join(&self.worktree, path)?;
        match std::fs::read(&full) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kontur-git-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let run = |args: &[&str]| {
            git(&p, args).unwrap();
        };
        git(&p, &["init", "-b", "main"]).unwrap();
        run(&["config", "user.email", "test@kontur.local"]);
        run(&["config", "user.name", "Kontur Test"]);
        std::fs::write(p.join("README.md"), "seed\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "seed"]);
        p
    }

    #[test]
    fn dirty_repo_prevents_create() {
        let repo = temp_repo();
        // Write an uncommitted file to make the repo dirty.
        std::fs::write(repo.join("dirty.txt"), "unsaved work\n").unwrap();
        let result = GitWorkspace::create(repo, "s-dirty");
        assert!(result.is_err(), "create should fail on a dirty repo");
        match result {
            Err(WorkspaceError::Io(msg)) => {
                assert!(msg.contains("dirty"), "error should mention dirty: {msg}");
            }
            Err(e) => panic!("unexpected error variant: {e:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn freeze_accept_and_merge_with_trailers() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo.clone(), "s1").unwrap();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "src/lib.rs", b"pub fn f() {}\n")
            .unwrap();
        let frozen = ws.freeze_task_diff(&t).unwrap();
        assert_eq!(frozen.files, vec!["src/lib.rs".to_string()]);
        assert!(frozen.loc >= 1);
        ws.accept_task(&t).unwrap();
        ws.merge_session("kontur session s1\n\nReviewed-by: A <a>\nReviewed-by: B <b>")
            .unwrap();
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

    #[test]
    fn read_file_returns_written_contents() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s-rf1").unwrap();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "src/lib.rs", b"pub fn f() {}\n")
            .unwrap();
        assert_eq!(
            ws.read_file(&t, "src/lib.rs").unwrap(),
            Some(b"pub fn f() {}\n".to_vec())
        );
    }

    #[test]
    fn read_file_returns_none_for_missing_path() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s-rf2").unwrap();
        let t = TaskId("t1".into());
        assert_eq!(ws.read_file(&t, "does_not_exist.rs").unwrap(), None);
    }

    #[test]
    fn apply_write_rejects_path_traversal() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s-sec1").unwrap();
        let t = TaskId("t1".into());
        let result = ws.apply_write(&t, "../escape.txt", b"x\n");
        assert!(result.is_err(), "traversal path must be rejected");
    }

    #[test]
    fn read_file_rejects_path_traversal() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s-sec2").unwrap();
        let t = TaskId("t1".into());
        let result = ws.read_file(&t, "../.ssh/id_ed25519");
        assert!(result.is_err(), "traversal read path must be rejected");
    }

    #[test]
    fn read_file_rejects_absolute_path() {
        let repo = temp_repo();
        let ws = GitWorkspace::create(repo, "s-sec3").unwrap();
        let t = TaskId("t1".into());
        let result = ws.read_file(&t, "/etc/passwd");
        assert!(result.is_err(), "absolute read path must be rejected");
    }
}
