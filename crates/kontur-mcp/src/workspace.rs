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
    fn apply_write(&self, task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), WorkspaceError>;
    fn run_command(&self, task_id: &TaskId, command: &str, cwd: &str) -> Result<CommandOutput, WorkspaceError>;
    /// Callers must not issue concurrent writes to the task between freeze and gate-open; the frozen diff is what operators sign against.
    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError>;
    fn accept_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError>;
    fn discard_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError>;
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
        InMemoryWorkspace { inner: Mutex::new(Inner::default()) }
    }

    pub fn accepted_tasks(&self) -> Vec<TaskId> {
        self.inner.lock().unwrap().accepted.iter().cloned().map(TaskId).collect()
    }

    pub fn discarded_tasks(&self) -> Vec<TaskId> {
        self.inner.lock().unwrap().discarded.iter().cloned().map(TaskId).collect()
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
    fn apply_write(&self, task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), WorkspaceError> {
        self.inner.lock().unwrap().task_mut(&task_id.0).writes.push((path.to_string(), contents.to_vec()));
        Ok(())
    }

    fn run_command(&self, task_id: &TaskId, command: &str, _cwd: &str) -> Result<CommandOutput, WorkspaceError> {
        self.inner.lock().unwrap().task_mut(&task_id.0).commands.push(command.to_string());
        Ok(CommandOutput { stdout: String::new(), exit_code: 0 })
    }

    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError> {
        let g = self.inner.lock().unwrap();
        let buf = g.task(&task_id.0).ok_or_else(|| WorkspaceError::UnknownTask(task_id.0.clone()))?;
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
}
