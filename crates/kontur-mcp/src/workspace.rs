use std::sync::Mutex;

use kontur_core::{sha256, Hash, TaskId};

use crate::error::WorkspaceError;

/// A frozen snapshot of a task's pending changes, ready to hash and review.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FrozenDiff {
    /// Canonical byte representation of the diff (what gets hashed).
    /// Encoding: for each file, path bytes, a NUL, contents, then LF.
    pub bytes: Vec<u8>,
    pub files: Vec<String>,
    pub loc: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub exit_code: i32,
}

/// The worktree side-effect port. The gate host owns an `Arc<dyn Workspace>`.
/// Implementations must be cheap to call under the session lock (sync, fast).
pub trait Workspace: Send + Sync {
    fn apply_write(
        &self,
        task_id: &TaskId,
        path: &str,
        contents: &[u8],
    ) -> Result<(), WorkspaceError>;
    fn run_command(
        &self,
        task_id: &TaskId,
        command: &str,
        cwd: &str,
    ) -> Result<CommandOutput, WorkspaceError>;
    /// Callers must not issue concurrent writes to the task between freeze and gate-open; the frozen diff is what operators sign against.
    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError>;
    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError>;
    fn discard_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError>;
    /// Session-end effect: land the approved session as one reviewed commit
    /// (real impls squash-merge the session branch; test doubles record it).
    /// Reachable only at session close.
    fn merge_session(&self, message: &str) -> Result<(), WorkspaceError>;
    /// Read the current contents of a file in the task's worktree.
    /// Returns `Ok(None)` when the path does not exist (new file that the agent
    /// has not yet written, or a path outside the task's writes).
    fn read_file(&self, task_id: &TaskId, path: &str) -> Result<Option<Vec<u8>>, WorkspaceError>;
    /// Where session audit chains should be persisted, if this workspace has a
    /// durable home for them (`<repo>/.kontur` for git workspaces). `None`
    /// (the default) means the chain is not written to disk.
    fn audit_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// The single source of a diff's hash — used at open, sign, and record time so
/// the verdict signatures bind to exactly this diff.
pub fn diff_hash(frozen: &FrozenDiff) -> Hash {
    sha256(&frozen.bytes)
}

#[derive(Default)]
struct TaskBuf {
    writes: Vec<(String, Vec<u8>)>,
    commands: Vec<String>,
}

#[derive(Default)]
struct Inner {
    tasks: Vec<(String, TaskBuf)>, // Vec, not HashMap: deterministic order
    accepted: Vec<String>,
    discarded: Vec<String>,
    merged: Option<String>,
}

impl Inner {
    fn task_mut(&mut self, id: &str) -> &mut TaskBuf {
        if !self.tasks.iter().any(|(t, _)| t == id) {
            self.tasks.push((id.to_string(), TaskBuf::default()));
        }
        &mut self.tasks.iter_mut().find(|(t, _)| t == id).unwrap().1
    }

    fn task(&self, id: &str) -> Option<&TaskBuf> {
        self.tasks.iter().find(|(t, _)| t == id).map(|(_, b)| b)
    }
}

/// In-memory workspace double: records writes/commands per task and reports a
/// deterministic frozen diff. Used by all orchestration tests.
pub struct InMemoryWorkspace {
    inner: Mutex<Inner>,
}

impl InMemoryWorkspace {
    pub fn new() -> Self {
        InMemoryWorkspace {
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn accepted_tasks(&self) -> Vec<TaskId> {
        self.inner
            .lock()
            .unwrap()
            .accepted
            .iter()
            .cloned()
            .map(TaskId)
            .collect()
    }

    pub fn discarded_tasks(&self) -> Vec<TaskId> {
        self.inner
            .lock()
            .unwrap()
            .discarded
            .iter()
            .cloned()
            .map(TaskId)
            .collect()
    }

    pub fn merged_message(&self) -> Option<String> {
        self.inner.lock().unwrap().merged.clone()
    }

    pub fn file_contents(&self, task_id: &TaskId, path: &str) -> Option<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.task(&task_id.0)?
            .writes
            .iter()
            .rev()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
    }
}

impl Default for InMemoryWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace for InMemoryWorkspace {
    fn apply_write(
        &self,
        task_id: &TaskId,
        path: &str,
        contents: &[u8],
    ) -> Result<(), WorkspaceError> {
        self.inner
            .lock()
            .unwrap()
            .task_mut(&task_id.0)
            .writes
            .push((path.to_string(), contents.to_vec()));
        Ok(())
    }

    fn run_command(
        &self,
        task_id: &TaskId,
        command: &str,
        _cwd: &str,
    ) -> Result<CommandOutput, WorkspaceError> {
        self.inner
            .lock()
            .unwrap()
            .task_mut(&task_id.0)
            .commands
            .push(command.to_string());
        Ok(CommandOutput {
            stdout: String::new(),
            exit_code: 0,
        })
    }

    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError> {
        let g = self.inner.lock().unwrap();
        let buf = g
            .task(&task_id.0)
            .ok_or_else(|| WorkspaceError::UnknownTask(task_id.0.clone()))?;
        // First-seen order of paths; last write wins per path — matches FsWorkspace
        // (which reads final on-disk content) so both impls produce the same diff hash.
        let mut files: Vec<String> = Vec::new();
        for (path, _) in &buf.writes {
            if !files.contains(path) {
                files.push(path.clone());
            }
        }
        let mut bytes = Vec::new();
        let mut loc = 0u32;
        for path in &files {
            let contents = buf
                .writes
                .iter()
                .rev()
                .find(|(p, _)| p == path)
                .map(|(_, c)| c.clone())
                .unwrap_or_default();
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&contents);
            bytes.push(b'\n');
            loc += contents.iter().filter(|b| **b == b'\n').count() as u32;
        }
        Ok(FrozenDiff { bytes, files, loc })
    }

    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        self.inner.lock().unwrap().accepted.push(task_id.0.clone());
        Ok(())
    }

    fn discard_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        self.inner.lock().unwrap().discarded.push(task_id.0.clone());
        Ok(())
    }

    fn merge_session(&self, message: &str) -> Result<(), WorkspaceError> {
        self.inner.lock().unwrap().merged = Some(message.to_string());
        Ok(())
    }

    fn read_file(&self, task_id: &TaskId, path: &str) -> Result<Option<Vec<u8>>, WorkspaceError> {
        let g = self.inner.lock().unwrap();
        // Return the last write for this path in this task, or None if not found.
        Ok(g.task(&task_id.0)
            .and_then(|buf| buf.writes.iter().rev().find(|(p, _)| p == path))
            .map(|(_, c)| c.clone()))
    }
}

/// Join `rel` onto `root`, refusing absolute paths and any traversal that
/// escapes the root. Purely lexical (the target may not exist yet): rejects
/// absolute paths and any `..` component. Case: `a/../b` inside root is
/// still rejected — keep it strict and simple.
///
/// Path containment is enforced here and relied on by GitWorkspace,
/// FsWorkspace `apply_write` and `read_file`. InMemoryWorkspace has no
/// filesystem — containment is not applicable there (no escape path).
pub fn contained_join(
    root: &std::path::Path,
    rel: &str,
) -> Result<std::path::PathBuf, WorkspaceError> {
    use std::path::Component;
    if rel.is_empty() {
        return Err(WorkspaceError::Io("path must not be empty".into()));
    }
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute() {
        return Err(WorkspaceError::Io(format!("path must be relative: {rel}")));
    }
    for component in rel_path.components() {
        match component {
            Component::ParentDir => {
                return Err(WorkspaceError::Io(format!(
                    "path traversal rejected: {rel}"
                )));
            }
            Component::Prefix(_) => {
                return Err(WorkspaceError::Io(format!(
                    "path prefix not allowed: {rel}"
                )));
            }
            _ => {}
        }
    }
    Ok(root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid() -> TaskId {
        TaskId("t1".into())
    }

    #[test]
    fn records_writes_and_freezes_deterministically() {
        let ws = InMemoryWorkspace::new();
        ws.apply_write(&tid(), "a.rs", b"line1\nline2\n").unwrap();
        ws.apply_write(&tid(), "b.rs", b"x\n").unwrap();
        let f1 = ws.freeze_task_diff(&tid()).unwrap();
        let f2 = ws.freeze_task_diff(&tid()).unwrap();
        assert_eq!(f1, f2);
        assert_eq!(f1.files, vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert_eq!(f1.loc, 3);
        assert_eq!(diff_hash(&f1), diff_hash(&f2));
    }

    #[test]
    fn accept_and_discard_are_observable() {
        let ws = InMemoryWorkspace::new();
        ws.apply_write(&tid(), "a.rs", b"x").unwrap();
        ws.accept_task(&tid()).unwrap();
        assert_eq!(ws.accepted_tasks(), vec![tid()]);
        assert!(ws.discarded_tasks().is_empty());
        assert_eq!(ws.file_contents(&tid(), "a.rs"), Some(b"x".to_vec()));
    }

    #[test]
    fn freeze_uses_last_write_per_path() {
        let ws = InMemoryWorkspace::new();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "a.rs", b"old\n").unwrap();
        ws.apply_write(&t, "a.rs", b"new\nnew2\n").unwrap();
        let f = ws.freeze_task_diff(&t).unwrap();
        assert_eq!(f.files, vec!["a.rs".to_string()]);
        assert_eq!(f.loc, 2); // last write's newline count; old write ignored
    }

    #[test]
    fn freeze_unknown_task_errors() {
        let ws = InMemoryWorkspace::new();
        assert_eq!(
            ws.freeze_task_diff(&TaskId("nope".into())).unwrap_err(),
            WorkspaceError::UnknownTask("nope".into())
        );
    }

    #[test]
    fn merge_session_records_message() {
        let ws = InMemoryWorkspace::new();
        assert!(ws.merged_message().is_none());
        ws.merge_session("session end\n\nReviewed-by: A").unwrap();
        assert_eq!(
            ws.merged_message(),
            Some("session end\n\nReviewed-by: A".to_string())
        );
    }

    #[test]
    fn read_file_returns_last_write() {
        let ws = InMemoryWorkspace::new();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "a.rs", b"v1\n").unwrap();
        ws.apply_write(&t, "a.rs", b"v2\n").unwrap();
        assert_eq!(ws.read_file(&t, "a.rs").unwrap(), Some(b"v2\n".to_vec()));
    }

    #[test]
    fn read_file_returns_none_for_unknown_path() {
        let ws = InMemoryWorkspace::new();
        let t = TaskId("t1".into());
        ws.apply_write(&t, "a.rs", b"x\n").unwrap();
        assert_eq!(ws.read_file(&t, "b.rs").unwrap(), None);
    }

    #[test]
    fn read_file_returns_none_for_unknown_task() {
        let ws = InMemoryWorkspace::new();
        // No writes at all — task doesn't exist.
        assert_eq!(ws.read_file(&TaskId("ghost".into()), "x.rs").unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // contained_join tests
    // -----------------------------------------------------------------------

    #[test]
    fn contained_join_accepts_normal_paths() {
        let root = std::path::Path::new("/repo");
        assert!(contained_join(root, "a/b.rs").is_ok());
        assert!(contained_join(root, "src/x/y.txt").is_ok());
        assert!(contained_join(root, "file.rs").is_ok());
    }

    #[test]
    fn contained_join_rejects_parent_traversal() {
        let root = std::path::Path::new("/repo");
        assert!(contained_join(root, "../x").is_err());
        assert!(contained_join(root, "a/../../x").is_err());
        assert!(contained_join(root, "a/../b").is_err());
    }

    #[test]
    fn contained_join_rejects_absolute_path() {
        let root = std::path::Path::new("/repo");
        assert!(contained_join(root, "/etc/passwd").is_err());
    }

    #[test]
    fn contained_join_rejects_empty() {
        let root = std::path::Path::new("/repo");
        assert!(contained_join(root, "").is_err());
    }
}
