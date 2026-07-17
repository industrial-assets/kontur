use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use kontur_core::TaskId;

use crate::error::WorkspaceError;
use crate::workspace::{CommandOutput, FrozenDiff, Workspace};

/// Filesystem-backed workspace: writes land under `root`, commands run via the
/// system shell. `accept_task` records acceptance (the real git commit/merge is
/// a later slice); `discard_task` removes the task's written files.
pub struct FsWorkspace {
    root: PathBuf,
    tracked: Mutex<Vec<(String, Vec<String>)>>, // (task_id, relative paths written)
}

impl FsWorkspace {
    pub fn new(root: PathBuf) -> Self {
        FsWorkspace { root, tracked: Mutex::new(Vec::new()) }
    }

    fn track(&self, task_id: &str, path: &str) {
        let mut g = self.tracked.lock().unwrap();
        if let Some((_, paths)) = g.iter_mut().find(|(t, _)| t == task_id) {
            if !paths.contains(&path.to_string()) {
                paths.push(path.to_string());
            }
        } else {
            g.push((task_id.to_string(), vec![path.to_string()]));
        }
    }

    fn paths_for(&self, task_id: &str) -> Vec<String> {
        self.tracked
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == task_id)
            .map(|(_, p)| p.clone())
            .unwrap_or_default()
    }
}

impl Workspace for FsWorkspace {
    fn apply_write(&self, task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), WorkspaceError> {
        let full = self.root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        }
        std::fs::write(&full, contents).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        self.track(&task_id.0, path);
        Ok(())
    }

    fn run_command(&self, _task_id: &TaskId, command: &str, cwd: &str) -> Result<CommandOutput, WorkspaceError> {
        let dir = if cwd.is_empty() { self.root.clone() } else { self.root.join(cwd) };
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&dir)
            .output()
            .map_err(|e| WorkspaceError::Io(e.to_string()))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    fn freeze_task_diff(&self, task_id: &TaskId) -> Result<FrozenDiff, WorkspaceError> {
        let paths = self.paths_for(&task_id.0);
        if paths.is_empty() {
            return Err(WorkspaceError::UnknownTask(task_id.0.clone()));
        }
        let mut bytes = Vec::new();
        let mut loc = 0u32;
        for path in &paths {
            let contents = std::fs::read(self.root.join(path)).map_err(|e| WorkspaceError::Io(e.to_string()))?;
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&contents);
            bytes.push(b'\n');
            loc += contents.iter().filter(|b| **b == b'\n').count() as u32;
        }
        Ok(FrozenDiff { bytes, files: paths, loc })
    }

    fn accept_task(&self, _task_id: &TaskId) -> Result<(), WorkspaceError> {
        // Acceptance recorded; the real git commit/merge is a later slice.
        Ok(())
    }

    fn discard_task(&self, task_id: &TaskId) -> Result<(), WorkspaceError> {
        for path in self.paths_for(&task_id.0) {
            let _ = std::fs::remove_file(self.root.join(path));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::diff_hash;

    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("kontur-fsws-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn writes_land_on_disk_and_freeze_is_stable() {
        let root = temp_root();
        let ws = FsWorkspace::new(root.clone());
        let task = TaskId("t1".into());
        ws.apply_write(&task, "src/a.txt", b"hello\n").unwrap();
        assert_eq!(std::fs::read(root.join("src/a.txt")).unwrap(), b"hello\n");
        let f1 = ws.freeze_task_diff(&task).unwrap();
        let f2 = ws.freeze_task_diff(&task).unwrap();
        assert_eq!(diff_hash(&f1), diff_hash(&f2));
        assert_eq!(f1.files, vec!["src/a.txt".to_string()]);
    }

    #[test]
    fn run_command_executes() {
        let ws = FsWorkspace::new(temp_root());
        let out = ws.run_command(&TaskId("t1".into()), "echo hi", "").unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hi"));
    }

    #[test]
    fn discard_removes_written_files() {
        let root = temp_root();
        let ws = FsWorkspace::new(root.clone());
        let task = TaskId("t2".into());
        ws.apply_write(&task, "gone.txt", b"x").unwrap();
        assert!(root.join("gone.txt").exists());
        ws.discard_task(&task).unwrap();
        assert!(!root.join("gone.txt").exists());
    }
}
