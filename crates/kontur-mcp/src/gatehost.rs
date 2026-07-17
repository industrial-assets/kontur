use std::sync::Arc;

use kontur_core::{
    reviewed_by as core_reviewed_by, verify_chain, Authorship, AuditChain, CastVerdict, ChainBreak,
    DualHold, GateId, GateRecord, HoldState, MakerSet, OperatorId, Provenance, Remedy, TaskId,
};
use tokio::sync::{watch, Mutex};

use crate::error::GateHostError;
use crate::session::SessionContext;
use crate::workspace::{CommandOutput, Workspace};

/// Result of a cast on the operator face.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GateProgress {
    pub state: HoldState,
    pub escalation_required: bool,
    /// Present only when the gate is `Blocked` — the remedy driving rework.
    pub remedy: Option<Remedy>,
}

struct HoldEntry {
    hold: DualHold,
    provenance: Provenance,
    watch_tx: watch::Sender<HoldState>,
    escalation_required: bool,
}

struct SessionState {
    ctx: SessionContext,
    chain: AuditChain,
    holds: Vec<HoldEntry>,
    next_gate: u64,
}

/// Owns session state behind a single lock and drives `kontur-core`.
pub struct GateHost {
    state: Arc<Mutex<SessionState>>,
    workspace: Arc<dyn Workspace>,
}

impl GateHost {
    pub fn new(ctx: SessionContext, workspace: Arc<dyn Workspace>) -> Self {
        GateHost {
            state: Arc::new(Mutex::new(SessionState {
                ctx,
                chain: AuditChain::new(),
                holds: Vec::new(),
                next_gate: 0,
            })),
            workspace,
        }
    }

    /// Agent face: record a worktree write on a task (not gated).
    pub async fn record_write(&self, task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), GateHostError> {
        self.workspace.apply_write(task_id, path, contents)?;
        Ok(())
    }

    /// Agent face: run a command in the worktree (not gated).
    pub async fn run_command(&self, task_id: &TaskId, command: &str, cwd: &str) -> Result<CommandOutput, GateHostError> {
        Ok(self.workspace.run_command(task_id, command, cwd)?)
    }

    /// Open a gate over a task's frozen diff. Returns the gate id and a receiver
    /// the awaiting agent-side handler watches for resolution.
    pub async fn open_gate(&self, task_id: TaskId, provenance: Provenance) -> (GateId, watch::Receiver<HoldState>) {
        let mut st = self.state.lock().await;
        st.next_gate += 1;
        let id = GateId(format!("gate-{:03}", st.next_gate));
        let hold = DualHold::new(
            id.clone(),
            task_id,
            provenance.diff_hash,
            st.ctx.policy,
            MakerSet::new(),
            Authorship::Agent,
        );
        let (tx, rx) = watch::channel(HoldState::Open);
        st.holds.push(HoldEntry { hold, provenance, watch_tx: tx, escalation_required: false });
        (id, rx)
    }

    /// Operator face: cast a signed verdict on a gate. On resolution, accepts or
    /// discards the task and publishes the new state on the gate's watch.
    pub async fn submit_verdict(&self, gate_id: &GateId, cv: CastVerdict) -> Result<GateProgress, GateHostError> {
        let mut st = self.state.lock().await;
        let idx = st
            .holds
            .iter()
            .position(|e| e.hold.gate_id() == gate_id)
            .ok_or_else(|| GateHostError::UnknownGate(gate_id.0.clone()))?;

        let ev = st.holds[idx].hold.version();
        let outcome = st.holds[idx].hold.cast(ev, cv)?;
        st.holds[idx].escalation_required = outcome.escalation_required;
        let state = outcome.state;

        let remedy = match state {
            HoldState::Satisfied => {
                let prev = st.chain.head();
                let (task_id, record) = {
                    let e = &st.holds[idx];
                    let rec = GateRecord::build(prev, e.provenance.clone(), &e.hold)
                        .expect("a satisfied hold always builds a record");
                    (e.hold.task_id().clone(), rec)
                };
                self.workspace.accept_task(&task_id)?;
                st.chain.append(record).expect("chain head matches prev by construction");
                None
            }
            HoldState::Blocked => {
                let (task_id, remedy) = {
                    let e = &st.holds[idx];
                    (e.hold.task_id().clone(), e.hold.blocking_remedy())
                };
                self.workspace.discard_task(&task_id)?;
                remedy
            }
            _ => None,
        };

        let _ = st.holds[idx].watch_tx.send(state);
        Ok(GateProgress { state, escalation_required: st.holds[idx].escalation_required, remedy })
    }

    /// Verify the whole audit chain (tamper-evidence check).
    pub async fn verify_audit(&self) -> Result<(), ChainBreak> {
        let st = self.state.lock().await;
        verify_chain(st.chain.records())
    }

    /// The operators whose verified go-signatures back a gate's record.
    pub async fn reviewed_by(&self, gate_id: &GateId) -> Option<Vec<OperatorId>> {
        let st = self.state.lock().await;
        st.chain
            .records()
            .iter()
            .find(|r| &r.core.gate_id == gate_id)
            .map(core_reviewed_by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::build_provenance;
    use crate::workspace::{diff_hash, InMemoryWorkspace, Workspace};
    use kontur_core::{Ed25519Signer, FixedClock, Hash, ReviewDepth, Signer, Verdict};

    fn ctx(ops: Vec<OperatorId>) -> SessionContext {
        SessionContext::new("do the thing", ops[0], "agent-01", "claude", "1.0", ops)
    }

    fn go_verdict(seed: u8, gate_id: &GateId, dh: Hash) -> CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        CastVerdict::create(&signer, &FixedClock(1000 + seed as i64), gate_id, dh, Verdict::Go, ReviewDepth::FullDiff, None)
    }

    async fn open_a_gate(host: &GateHost, ws: &InMemoryWorkspace, ctx: &SessionContext) -> (GateId, Hash) {
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let frozen = ws.freeze_task_diff(&task).unwrap();
        let dh = diff_hash(&frozen);
        let prov = build_provenance(ctx, &task, dh, &frozen, 100);
        let (gid, _rx) = host.open_gate(task, prov).await;
        (gid, dh)
    }

    #[tokio::test]
    async fn two_go_verdicts_satisfy_and_accept() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        let p1 = host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        assert_eq!(p1.state, HoldState::Partial);
        assert!(ws.accepted_tasks().is_empty());

        let p2 = host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();
        assert_eq!(p2.state, HoldState::Satisfied);

        assert_eq!(ws.accepted_tasks(), vec![TaskId("t1".into())]);
        assert!(host.verify_audit().await.is_ok());
        assert_eq!(host.reviewed_by(&gid).await.unwrap().len(), 2);
    }
}
