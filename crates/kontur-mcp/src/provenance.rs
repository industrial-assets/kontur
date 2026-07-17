use kontur_core::{Hash, Provenance, TaskId};

use crate::session::SessionContext;
use crate::workspace::FrozenDiff;

/// Assemble a `kontur_core::Provenance` for a gate from the session context and
/// the frozen task diff. Note: `Provenance` has no tool-trail field — the
/// tool-trail is recorded on the workspace, not folded into the signed record.
pub fn build_provenance(
    ctx: &SessionContext,
    task_id: &TaskId,
    diff_hash: Hash,
    frozen: &FrozenDiff,
    tokens: u64,
) -> Provenance {
    Provenance {
        task_id: task_id.clone(),
        prompt: ctx.prompt.clone(),
        prompt_author: ctx.prompt_author,
        agent_id: ctx.agent_id.clone(),
        agent_model: ctx.agent_model.clone(),
        agent_version: ctx.agent_version.clone(),
        diff_hash,
        files: frozen.files.clone(),
        loc: frozen.loc,
        tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{diff_hash, InMemoryWorkspace, Workspace};
    use kontur_core::OperatorId;

    #[test]
    fn maps_session_and_diff_fields() {
        let ws = InMemoryWorkspace::new();
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let frozen = ws.freeze_task_diff(&task).unwrap();
        let dh = diff_hash(&frozen);

        let ctx = SessionContext::new("do it", OperatorId([1; 32]), "agent-01", "claude", "1.0", vec![OperatorId([1; 32])]);
        let p = build_provenance(&ctx, &task, dh, &frozen, 6400);

        assert_eq!(p.task_id, task);
        assert_eq!(p.prompt, "do it");
        assert_eq!(p.agent_id, "agent-01");
        assert_eq!(p.diff_hash, dh);
        assert_eq!(p.files, vec!["a.rs".to_string()]);
        assert_eq!(p.loc, 1);
        assert_eq!(p.tokens, 6400);
    }
}
