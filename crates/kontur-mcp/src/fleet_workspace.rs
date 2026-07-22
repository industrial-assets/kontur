use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kontur_core::TaskId;

use crate::error::WorkspaceError;
use crate::git_workspace::{git, GitWorkspace};
use crate::workspace::{CommandOutput, FrozenDiff, Workspace};

/// Per-agent worktree isolation for a fleet. Each agent gets its own
/// `GitWorkspace` (branch `kontur/<session>-<agent>` in a dedicated worktree),
/// so one agent's uncommitted writes can never leak into another agent's gated
/// diff — the `git add -A` in `freeze_task_diff` only ever sees a single
/// agent's worktree.
///
/// Routing is by task: the `GateHost` calls [`Workspace::assign_task`] to bind
/// a task to its agent before the task's first write, and every task-keyed
/// operation is then dispatched to that agent's worktree. A task with no
/// recorded owner falls back to the `default_agent` (the session's primary
/// agent), so a degenerate single-agent fleet behaves like one `GitWorkspace`.
///
/// Aggregate merge semantics (combining every agent's branch into a single
/// reviewed commit, and cross-agent conflict handling) are intentionally left
/// to the aggregate-merge PR; here `merge_session` lands each non-empty agent
/// branch in turn.
pub struct FleetWorkspace {
    repo: PathBuf,
    session: String,
    default_agent: String,
    /// The base branch the session was launched from — each agent branch is
    /// squash-merged back into it at session close.
    base: String,
    inner: Mutex<FleetInner>,
}

#[derive(Default)]
struct FleetInner {
    /// agent id -> its worktree. Vec (not map) for deterministic merge order.
    agents: Vec<(String, GitWorkspace)>,
    /// task id -> owning agent. First assignment wins.
    task_owner: Vec<(String, String)>,
}

impl FleetInner {
    /// The agent that owns a task, or `default` when unassigned.
    fn owner(&self, task: &str, default: &str) -> String {
        self.task_owner
            .iter()
            .find(|(t, _)| t == task)
            .map(|(_, a)| a.clone())
            .unwrap_or_else(|| default.to_string())
    }

    /// Return the agent's worktree, creating it (branch + worktree) on first
    /// use.
    fn ensure_agent(
        &mut self,
        agent: &str,
        repo: &Path,
        session: &str,
    ) -> Result<&GitWorkspace, WorkspaceError> {
        if !self.agents.iter().any(|(a, _)| a == agent) {
            let ws = GitWorkspace::create(repo.to_path_buf(), &format!("{session}-{agent}"))?;
            self.agents.push((agent.to_string(), ws));
        }
        Ok(&self.agents.iter().find(|(a, _)| a == agent).unwrap().1)
    }
}

impl FleetWorkspace {
    /// Launch a fleet workspace rooted at `repo`. The `default_agent`'s worktree
    /// is created eagerly, which also runs the dirty-repo pre-flight check (the
    /// squash-merge at close cannot conflict with uncommitted work).
    pub fn create(
        repo: PathBuf,
        session: &str,
        default_agent: &str,
    ) -> Result<Self, WorkspaceError> {
        let base = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string();
        let default_ws = GitWorkspace::create(repo.clone(), &format!("{session}-{default_agent}"))?;
        Ok(FleetWorkspace {
            repo,
            session: session.to_string(),
            default_agent: default_agent.to_string(),
            base,
            inner: Mutex::new(FleetInner {
                agents: vec![(default_agent.to_string(), default_ws)],
                task_owner: Vec::new(),
            }),
        })
    }
}

impl Workspace for FleetWorkspace {
    fn assign_task(&self, task_id: &TaskId, agent: &str) {
        let mut g = self.inner.lock().unwrap();
        if !g.task_owner.iter().any(|(t, _)| t == &task_id.0) {
            g.task_owner.push((task_id.0.clone(), agent.to_string()));
        }
    }

    fn apply_write(
        &self,
        task_id: &TaskId,
        path: &str,
        contents: &[u8],
    ) -> Result<(), WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.apply_write(task_id, path, contents)
    }

    fn run_command(
        &self,
        task_id: &TaskId,
        command: &str,
        cwd: &str,
    ) -> Result<CommandOutput, WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.run_command(task_id, command, cwd)
    }

    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.freeze_task_diff(task_id)
    }

    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.accept_task(task_id)
    }

    fn discard_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.discard_task(task_id)
    }

    fn read_file(&self, task_id: &TaskId, path: &str) -> Result<Option<Vec<u8>>, WorkspaceError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.owner(&task_id.0, &self.default_agent);
        let ws = g.ensure_agent(&agent, &self.repo, &self.session)?;
        ws.read_file(task_id, path)
    }

    /// Land each agent's approved work. Every non-empty agent branch is
    /// squash-merged back into the base branch in agent order (each as its own
    /// commit); empty branches (an agent that produced nothing, or whose work
    /// was all discarded) are skipped so an empty merge cannot error. Combining
    /// these into a single aggregate commit is the aggregate-merge PR's job.
    fn merge_session(&self, message: &str) -> Result<(), WorkspaceError> {
        let g = self.inner.lock().unwrap();
        for (_agent, ws) in &g.agents {
            // Skip a branch with no commits beyond base — nothing to merge.
            let count = git(
                &self.repo,
                &[
                    "rev-list",
                    "--count",
                    &format!("{}..{}", self.base, ws.branch()),
                ],
            )?;
            if count.trim() == "0" {
                continue;
            }
            ws.merge_session(message)?;
        }
        Ok(())
    }

    fn audit_dir(&self) -> Option<PathBuf> {
        Some(self.repo.join(".kontur"))
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
            "kontur-fleet-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        git(&p, &["init", "-b", "main"]).unwrap();
        git(&p, &["config", "user.email", "test@kontur.local"]).unwrap();
        git(&p, &["config", "user.name", "Kontur Test"]).unwrap();
        std::fs::write(p.join("README.md"), "seed\n").unwrap();
        git(&p, &["add", "-A"]).unwrap();
        git(&p, &["commit", "-m", "seed"]).unwrap();
        p
    }

    /// The core isolation property: two agents each write a file under the same
    /// logical task path, and each agent's frozen diff contains ONLY its own
    /// write — no leakage through `git add -A`.
    #[test]
    fn two_agents_have_isolated_frozen_diffs() {
        let repo = temp_repo();
        let fw = FleetWorkspace::create(repo, "s1", "agent-a").unwrap();

        let ta = TaskId("agent-a::1".into());
        let tb = TaskId("agent-b::1".into());
        fw.assign_task(&ta, "agent-a");
        fw.assign_task(&tb, "agent-b");

        fw.apply_write(&ta, "shared.rs", b"from A\n").unwrap();
        fw.apply_write(&tb, "shared.rs", b"from B\n").unwrap();

        let fa = fw.freeze_task_diff(&ta).unwrap();
        let fb = fw.freeze_task_diff(&tb).unwrap();

        // Each diff touches exactly one file, and carries only that agent's bytes.
        assert_eq!(fa.files, vec!["shared.rs".to_string()]);
        assert_eq!(fb.files, vec!["shared.rs".to_string()]);
        let a_txt = String::from_utf8(fa.bytes).unwrap();
        let b_txt = String::from_utf8(fb.bytes).unwrap();
        assert!(a_txt.contains("from A"), "A's diff must contain A's write");
        assert!(
            !a_txt.contains("from B"),
            "A's diff must NOT contain B's write"
        );
        assert!(b_txt.contains("from B"), "B's diff must contain B's write");
        assert!(
            !b_txt.contains("from A"),
            "B's diff must NOT contain A's write"
        );
    }

    /// An unassigned task falls back to the default agent's worktree.
    #[test]
    fn unassigned_task_routes_to_default_agent() {
        let repo = temp_repo();
        let fw = FleetWorkspace::create(repo, "s2", "agent-a").unwrap();
        let t = TaskId("no-owner".into());
        fw.apply_write(&t, "x.rs", b"hi\n").unwrap();
        assert_eq!(fw.read_file(&t, "x.rs").unwrap(), Some(b"hi\n".to_vec()));
        let f = fw.freeze_task_diff(&t).unwrap();
        assert_eq!(f.files, vec!["x.rs".to_string()]);
    }

    /// accept + merge lands both agents' work on the base branch.
    #[test]
    fn merge_lands_every_agents_accepted_work() {
        let repo = temp_repo();
        let fw = FleetWorkspace::create(repo.clone(), "s3", "agent-a").unwrap();

        let ta = TaskId("agent-a::1".into());
        let tb = TaskId("agent-b::1".into());
        fw.assign_task(&ta, "agent-a");
        fw.assign_task(&tb, "agent-b");
        fw.apply_write(&ta, "a.rs", b"pub fn a() {}\n").unwrap();
        fw.apply_write(&tb, "b.rs", b"pub fn b() {}\n").unwrap();
        fw.accept_task(&ta).unwrap();
        fw.accept_task(&tb).unwrap();

        fw.merge_session("kontur session s3\n\nReviewed-by: A <a>\nReviewed-by: B <b>")
            .unwrap();

        // Both files are present on main after merge.
        assert_eq!(
            std::fs::read(repo.join("a.rs")).unwrap(),
            b"pub fn a() {}\n"
        );
        assert_eq!(
            std::fs::read(repo.join("b.rs")).unwrap(),
            b"pub fn b() {}\n"
        );
        let log = git(&repo, &["log", "--format=%B", "main"]).unwrap();
        assert!(log.contains("Reviewed-by: A <a>"));
    }

    /// discard on one agent's task does not disturb the other agent's worktree.
    #[test]
    fn discard_is_per_agent() {
        let repo = temp_repo();
        let fw = FleetWorkspace::create(repo, "s4", "agent-a").unwrap();
        let ta = TaskId("agent-a::1".into());
        let tb = TaskId("agent-b::1".into());
        fw.assign_task(&ta, "agent-a");
        fw.assign_task(&tb, "agent-b");
        fw.apply_write(&ta, "a.rs", b"A\n").unwrap();
        fw.apply_write(&tb, "b.rs", b"B\n").unwrap();

        fw.discard_task(&ta).unwrap();

        // A's write is gone; B's remains.
        assert_eq!(fw.read_file(&ta, "a.rs").unwrap(), None);
        assert_eq!(fw.read_file(&tb, "b.rs").unwrap(), Some(b"B\n".to_vec()));
    }

    /// A dirty repo is refused up front (via the eager default-agent worktree).
    #[test]
    fn dirty_repo_prevents_create() {
        let repo = temp_repo();
        std::fs::write(repo.join("dirty.txt"), "unsaved\n").unwrap();
        assert!(FleetWorkspace::create(repo, "s5", "agent-a").is_err());
    }

    /// merge with an agent that produced nothing skips the empty branch cleanly.
    #[test]
    fn merge_skips_empty_agent_branch() {
        let repo = temp_repo();
        let fw = FleetWorkspace::create(repo.clone(), "s6", "agent-a").unwrap();
        // agent-b is assigned and touched, but never accepts anything.
        let ta = TaskId("agent-a::1".into());
        let tb = TaskId("agent-b::1".into());
        fw.assign_task(&ta, "agent-a");
        fw.assign_task(&tb, "agent-b");
        fw.apply_write(&ta, "a.rs", b"A\n").unwrap();
        fw.apply_write(&tb, "b.rs", b"B\n").unwrap();
        fw.accept_task(&ta).unwrap();
        // agent-b's write is never accepted -> its branch has no commits.

        fw.merge_session("only A\n\nReviewed-by: A <a>").unwrap();
        assert!(repo.join("a.rs").exists());
        assert!(!repo.join("b.rs").exists());
    }
}
