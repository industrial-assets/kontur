use std::sync::Arc;

use kontur_core::{
    reviewed_by as core_reviewed_by, verify_chain, Authorship, AuditChain, CastVerdict, ChainBreak,
    DualHold, GateId, GateRecord, Hash, HoldState, MakerSet, OperatorId, Provenance, Remedy,
    TaskId, VerdictView,
};
use tokio::sync::{watch, Mutex};

use crate::error::GateHostError;
use crate::provenance::build_provenance;
use crate::session::SessionContext;
use crate::workspace::{diff_hash, CommandOutput, Workspace};

/// Result of a cast on the operator face.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GateProgress {
    pub state: HoldState,
    pub escalation_required: bool,
    /// Present only when the gate is `Blocked` — the remedy driving rework.
    pub remedy: Option<Remedy>,
}

/// Operator-face projection of a pending gate. Never exposes a sealed verdict
/// value — `observed` is `kontur-core`'s sealing-safe `VerdictView`.
#[derive(Clone, Debug)]
pub struct GateView {
    pub gate_id: GateId,
    pub task_id: TaskId,
    pub diff_hash: Hash,
    pub state: HoldState,
    pub observed: Vec<VerdictView>,
    pub escalation_required: bool,
}

/// Terminal summary of a gate, read by the awaiting agent handler.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GateFinal {
    pub state: HoldState,
    pub remedy: Option<Remedy>,
    pub reviewed_by: Vec<OperatorId>,
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

    /// Agent face: freeze the task diff, build provenance, and open its gate.
    /// Composes the workspace + provenance so the server stays thin.
    pub async fn begin_task_gate(
        &self,
        task_id: TaskId,
        tokens: u64,
    ) -> Result<(GateId, watch::Receiver<HoldState>), GateHostError> {
        let frozen = self.workspace.freeze_task_diff(&task_id)?;
        let dh = diff_hash(&frozen);
        let provenance = {
            let st = self.state.lock().await;
            build_provenance(&st.ctx, &task_id, dh, &frozen, tokens)
        };
        Ok(self.open_gate(task_id, provenance).await)
    }

    /// Operator face: gates awaiting review, sealing-safe.
    pub async fn pending_gates(&self) -> Vec<GateView> {
        let st = self.state.lock().await;
        st.holds
            .iter()
            .filter(|e| matches!(e.hold.state(), HoldState::Open | HoldState::Partial))
            .map(|e| GateView {
                gate_id: e.hold.gate_id().clone(),
                task_id: e.hold.task_id().clone(),
                diff_hash: e.hold.diff_hash(),
                state: e.hold.state(),
                observed: e.hold.observed_verdicts(),
                escalation_required: e.escalation_required,
            })
            .collect()
    }

    /// Read a gate's terminal outcome (for the awaiting agent handler).
    pub async fn gate_outcome(&self, gate_id: &GateId) -> Option<GateFinal> {
        let st = self.state.lock().await;
        let e = st.holds.iter().find(|e| e.hold.gate_id() == gate_id)?;
        let state = e.hold.state();
        let remedy = e.hold.blocking_remedy();
        let reviewed_by = st
            .chain
            .records()
            .iter()
            .find(|r| &r.core.gate_id == gate_id)
            .map(core_reviewed_by)
            .unwrap_or_default();
        Some(GateFinal { state, remedy, reviewed_by })
    }

    /// Operator face: a hand-edit. Applies to the worktree immediately, then
    /// opens a FRESH gate over the combined diff (deferred acceptance). The
    /// editor joins the maker set (strict mode excludes them); escalation is
    /// signalled on the first cast when the eligible pool < required.
    pub async fn hand_edit(
        &self,
        task_id: TaskId,
        path: &str,
        contents: &[u8],
        editor: OperatorId,
    ) -> Result<GateId, GateHostError> {
        self.workspace.apply_write(&task_id, path, contents)?;
        let frozen = self.workspace.freeze_task_diff(&task_id)?;
        let dh = diff_hash(&frozen);

        let mut st = self.state.lock().await;
        st.next_gate += 1;
        let id = GateId(format!("gate-{:03}", st.next_gate));
        let provenance = build_provenance(&st.ctx, &task_id, dh, &frozen, 0);
        let hold = DualHold::reopen_handedit(
            id.clone(),
            task_id,
            dh,
            st.ctx.policy,
            MakerSet::new(),
            editor,
            true,
            &st.ctx.operators,
        );
        let (tx, _rx) = watch::channel(hold.state());
        st.holds.push(HoldEntry { hold, provenance, watch_tx: tx, escalation_required: false });
        Ok(id)
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

    fn nogo_verdict(seed: u8, gate_id: &GateId, dh: Hash, steer: &str) -> CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        CastVerdict::create(
            &signer,
            &FixedClock(2000 + seed as i64),
            gate_id,
            dh,
            Verdict::NoGo(kontur_core::Remedy::Steer(steer.into())),
            ReviewDepth::FullDiff,
            None,
        )
    }

    #[tokio::test]
    async fn nogo_blocks_discards_and_returns_remedy() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        let p2 = host.submit_verdict(&gid, nogo_verdict(2, &gid, dh, "cache it")).await.unwrap();

        assert_eq!(p2.state, HoldState::Blocked);
        assert_eq!(p2.remedy, Some(kontur_core::Remedy::Steer("cache it".into())));
        assert!(ws.accepted_tasks().is_empty());
        assert_eq!(ws.discarded_tasks(), vec![TaskId("t1".into())]);
    }

    #[tokio::test]
    async fn pending_gates_hides_sealed_first_verdict() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        let pending = host.pending_gates().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, HoldState::Partial);
        assert_eq!(pending[0].observed.len(), 1);
        assert_eq!(pending[0].observed[0].status, kontur_core::VerdictStatus::Sealed);
        assert_eq!(pending[0].diff_hash, dh);
    }

    #[tokio::test]
    async fn begin_task_gate_and_outcome_reports_satisfied() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (gid, _rx) = host.begin_task_gate(task, 42).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();

        let outcome = host.gate_outcome(&gid).await.unwrap();
        assert_eq!(outcome.state, HoldState::Satisfied);
        assert_eq!(outcome.reviewed_by.len(), 2);
        assert!(outcome.remedy.is_none());
    }

    #[tokio::test]
    async fn hand_edit_applies_now_and_opens_fresh_gate() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        // Pragmatic policy so the editor (op1) may co-sign.
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        let gid = host.hand_edit(task.clone(), "a.rs", b"guarded\n", op1).await.unwrap();
        // Applied immediately, observable in the workspace.
        assert_eq!(ws.file_contents(&task, "a.rs"), Some(b"guarded\n".to_vec()));

        let dh = host.pending_gates().await[0].diff_hash;
        // Editor op1 co-signs (pragmatic), op2 co-signs -> satisfied.
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        let p = host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();
        assert_eq!(p.state, HoldState::Satisfied);
        assert_eq!(ws.accepted_tasks(), vec![task]);
    }

    #[tokio::test]
    async fn hand_edit_strict_signals_escalation_and_excludes_editor() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        // Default policy = strict.
        let host = GateHost::new(ctx(vec![op1, op2]), ws.clone());

        let task = TaskId("t1".into());
        let gid = host.hand_edit(task, "a.rs", b"guarded\n", op1).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // op2 (non-editor) casts: accepted, but escalation is signalled (pool = 1 < 2).
        let p = host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();
        assert!(p.escalation_required);

        // op1 (the editor) is a maker in strict mode -> rejected.
        let err = host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap_err();
        assert_eq!(err, GateHostError::Cast(kontur_core::CastRejected::Ineligible));
    }
}
