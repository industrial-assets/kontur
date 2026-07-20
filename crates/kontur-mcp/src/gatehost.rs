use std::sync::Arc;

use kontur_core::{
    reviewed_by as core_reviewed_by, verify_chain, Authorship, AuditChain, CastVerdict, ChainBreak,
    DualHold, GateId, GateRecord, Hash, HoldState, MakerSet, OperatorId, Provenance, Remedy,
    TaskId, VerdictView,
};
use tokio::sync::{broadcast, watch, Mutex};

use crate::error::GateHostError;
use crate::provenance::build_provenance;
use crate::session::SessionContext;
use crate::workspace::{diff_hash, CommandOutput, Workspace};

/// Live activity events for observers (the session server). Best-effort
/// display stream — never blocks or gates the enforcement path.
#[derive(Clone, Debug)]
pub enum HostEvent {
    Write { task: TaskId, path: String, bytes: usize },
    Command { task: TaskId, command: String },
    GateOpened { gate_id: GateId, task: TaskId },
    GateResolved { gate_id: GateId, state: HoldState },
    PlanProposed { tasks: Vec<String> },
    SessionAbandoned,
}

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
    pub files: Vec<String>,
    pub loc: u32,
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
    plan: Option<Vec<String>>,
    plan_approved_tx: watch::Sender<bool>,
    /// Kept alive so `send_replace` on `plan_approved_tx` is never a no-op
    /// (watch::send discards when there are zero receivers; keeping one here
    /// guarantees the channel is always live — same pattern as kontur-net).
    _plan_approved_rx: watch::Receiver<bool>,
    /// Set by `abandon_session` under the state lock. Once `true`,
    /// `submit_verdict`, `begin_task_gate`, `hand_edit`, and `propose_plan`
    /// all return `Err(GateHostError::SessionAbandoned)` immediately.
    ///
    /// Coherence: a cast that beats the flag commits before discard → discard
    /// resets to the new HEAD (harmless, audit coherent). A cast after the
    /// flag is refused → no accept post-abandon.
    abandoned: bool,
}

/// Owns session state behind a single lock and drives `kontur-core`.
pub struct GateHost {
    state: Arc<Mutex<SessionState>>,
    workspace: Arc<dyn Workspace>,
    events: broadcast::Sender<HostEvent>,
}

impl GateHost {
    pub fn new(ctx: SessionContext, workspace: Arc<dyn Workspace>) -> Self {
        let (events, _) = broadcast::channel(64);
        let (plan_approved_tx, _plan_approved_rx) = watch::channel(false);
        GateHost {
            state: Arc::new(Mutex::new(SessionState {
                ctx,
                chain: AuditChain::new(),
                holds: Vec::new(),
                next_gate: 0,
                plan: None,
                plan_approved_tx,
                _plan_approved_rx,
                abandoned: false,
            })),
            workspace,
            events,
        }
    }

    /// Subscribe to live host activity events (display-only, best-effort).
    pub fn subscribe_events(&self) -> broadcast::Receiver<HostEvent> {
        self.events.subscribe()
    }

    /// Agent face: propose a plan (list of task descriptions). Stores the plan,
    /// emits `HostEvent::PlanProposed`, and returns a watch receiver that flips
    /// to `true` when both operators approve. Re-proposal overwrites (idempotent).
    pub async fn propose_plan(&self, tasks: Vec<String>) -> Result<watch::Receiver<bool>, GateHostError> {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
        // BUG CLASS: approval state lifetime must match proposal, not session.
        // Create a fresh watch channel on each proposal. Prior subscribers are
        // closed (their changed() errors), correctly surfacing "plan superseded".
        // This prevents the stale-approval bypass: after approve_plan() sets
        // `true`, a re-proposal returning a subscriber from the *old* channel
        // would immediately read true without operator action.
        let (new_tx, new_rx) = watch::channel(false);
        st.plan_approved_tx = new_tx;
        st._plan_approved_rx = new_rx;

        st.plan = Some(tasks.clone());
        // Return a new subscriber BEFORE releasing the lock so the initial
        // `false` is always visible and `send_replace(true)` from approve_plan
        // cannot race past this subscribe.
        let rx = st.plan_approved_tx.subscribe();
        drop(st);
        let _ = self.events.send(HostEvent::PlanProposed { tasks });
        Ok(rx)
    }

    /// Operator face: mark the proposed plan as approved, unblocking any
    /// awaiter on the watch returned by `propose_plan`.
    pub async fn approve_plan(&self) {
        let st = self.state.lock().await;
        // send_replace never discards (we keep _plan_approved_rx alive in state).
        st.plan_approved_tx.send_replace(true);
    }

    /// Operator face: read the currently proposed plan (None until one arrives).
    pub async fn proposed_plan(&self) -> Option<Vec<String>> {
        self.state.lock().await.plan.clone()
    }

    /// The agent id for this session.
    pub async fn agent_id(&self) -> String {
        self.state.lock().await.ctx.agent_id.clone()
    }

    /// Operator face: update the session prompt. Called when an operator edits
    /// the prompt in-console during DispatchReady. Subsequent gate provenance
    /// (built from ctx.prompt at gate-open time) carries the updated text.
    pub async fn set_prompt(&self, prompt: String) {
        self.state.lock().await.ctx.prompt = prompt;
    }

    /// Agent face: record a worktree write on a task (not gated).
    pub async fn record_write(&self, task_id: &TaskId, path: &str, contents: &[u8]) -> Result<(), GateHostError> {
        self.workspace.apply_write(task_id, path, contents)?;
        let _ = self.events.send(HostEvent::Write {
            task: task_id.clone(),
            path: path.to_owned(),
            bytes: contents.len(),
        });
        Ok(())
    }

    /// Agent face: run a command in the worktree (not gated).
    pub async fn run_command(&self, task_id: &TaskId, command: &str, cwd: &str) -> Result<CommandOutput, GateHostError> {
        let out = self.workspace.run_command(task_id, command, cwd)?;
        let _ = self.events.send(HostEvent::Command {
            task: task_id.clone(),
            command: command.to_owned(),
        });
        Ok(out)
    }

    /// Open a gate over a task's frozen diff. Returns the gate id and a receiver
    /// the awaiting agent-side handler watches for resolution.
    pub async fn open_gate(&self, task_id: TaskId, provenance: Provenance) -> (GateId, watch::Receiver<HoldState>) {
        let mut st = self.state.lock().await;
        st.next_gate += 1;
        let id = GateId(format!("gate-{:03}", st.next_gate));
        let task_id_for_event = task_id.clone();
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
        drop(st);
        let _ = self.events.send(HostEvent::GateOpened {
            gate_id: id.clone(),
            task: task_id_for_event,
        });
        (id, rx)
    }

    /// Operator face: cast a signed verdict on a gate. On resolution, accepts or
    /// discards the task and publishes the new state on the gate's watch.
    pub async fn submit_verdict(&self, gate_id: &GateId, cv: CastVerdict) -> Result<GateProgress, GateHostError> {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
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
                let prev = st.chain.head();
                let (task_id, remedy, record) = {
                    let e = &st.holds[idx];
                    let rec = GateRecord::build(prev, e.provenance.clone(), &e.hold)
                        .expect("a resolved hold always builds a record");
                    (e.hold.task_id().clone(), e.hold.blocking_remedy(), rec)
                };
                st.chain.append(record).expect("chain head matches prev by construction");
                self.workspace.discard_task(&task_id)?;
                remedy
            }
            _ => None,
        };

        let escalation_required = st.holds[idx].escalation_required;
        let _ = st.holds[idx].watch_tx.send(state);
        let gate_id_for_event = gate_id.clone();
        drop(st);
        if matches!(state, HoldState::Satisfied | HoldState::Blocked) {
            let _ = self.events.send(HostEvent::GateResolved {
                gate_id: gate_id_for_event,
                state,
            });
        }
        Ok(GateProgress { state, escalation_required, remedy })
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
            if st.abandoned {
                return Err(GateHostError::SessionAbandoned);
            }
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
                files: e.provenance.files.clone(),
                loc: e.provenance.loc,
            })
            .collect()
    }

    /// Read a gate's terminal outcome (for the awaiting agent handler).
    /// Returns Some for a gate in ANY state; callers must inspect `state` before acting.
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
    ) -> Result<(GateId, watch::Receiver<HoldState>), GateHostError> {
        {
            let st = self.state.lock().await;
            if st.abandoned {
                return Err(GateHostError::SessionAbandoned);
            }
        }
        self.workspace.apply_write(&task_id, path, contents)?;
        let frozen = self.workspace.freeze_task_diff(&task_id)?;
        let dh = diff_hash(&frozen);

        let mut st = self.state.lock().await;
        st.next_gate += 1;
        let id = GateId(format!("gate-{:03}", st.next_gate));
        let task_id_for_event = task_id.clone();
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
        let (tx, rx) = watch::channel(hold.state());
        let escalation_required = hold.escalation_required();
        st.holds.push(HoldEntry { hold, provenance, watch_tx: tx, escalation_required });
        drop(st);
        let _ = self.events.send(HostEvent::GateOpened {
            gate_id: id.clone(),
            task: task_id_for_event,
        });
        Ok((id, rx))
    }

    /// The current frozen diff bytes for a gate's task (a review preview). The
    /// authoritative content hash is the gate's `diff_hash`; this is the bytes
    /// an operator opens to review.
    pub async fn gate_diff(&self, gate_id: &GateId) -> Option<Vec<u8>> {
        let st = self.state.lock().await;
        let task_id = st.holds.iter().find(|e| e.hold.gate_id() == gate_id)?.hold.task_id().clone();
        drop(st);
        self.workspace.freeze_task_diff(&task_id).ok().map(|f| f.bytes)
    }

    /// Number of records currently in the audit chain.
    pub async fn audit_len(&self) -> usize {
        self.state.lock().await.chain.records().len()
    }

    /// Session-end: land the approved work as one reviewed commit.
    pub async fn merge_session(&self, message: &str) -> Result<(), GateHostError> {
        Ok(self.workspace.merge_session(message)?)
    }

    /// Operator kill-switch: discard all pending (Open/Partial) tasks and emit
    /// `SessionAbandoned`. Already-resolved gates keep their audit records.
    /// Nothing is merged.
    ///
    /// Coherence: `abandoned` is set to `true` under the state lock before
    /// discards run. A concurrent `submit_verdict` that beats the flag commits
    /// before `discard_task` runs → `discard_task` resets the worktree to the
    /// new HEAD (harmless, audit coherent). A `submit_verdict` that loses the
    /// race observes `abandoned = true` and returns `Err(SessionAbandoned)` →
    /// no accept post-abandon.
    pub async fn abandon_session(&self) -> Result<(), GateHostError> {
        let task_ids: Vec<TaskId> = {
            let mut st = self.state.lock().await;
            st.abandoned = true;
            st.holds
                .iter()
                .filter(|e| matches!(e.hold.state(), HoldState::Open | HoldState::Partial))
                .map(|e| e.hold.task_id().clone())
                .collect()
        };
        for task_id in task_ids {
            self.workspace.discard_task(&task_id)?;
        }
        let _ = self.events.send(HostEvent::SessionAbandoned);
        Ok(())
    }

    /// The session's operator roster (for composing Reviewed-by trailers).
    pub async fn session_operators(&self) -> Vec<OperatorId> {
        self.state.lock().await.ctx.operators.clone()
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
        assert_eq!(host.audit_len().await, 1);
        assert!(host.verify_audit().await.is_ok());
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

        let (gid, _rx) = host.hand_edit(task.clone(), "a.rs", b"guarded\n", op1).await.unwrap();
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
        let (gid, _rx) = host.hand_edit(task, "a.rs", b"guarded\n", op1).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // op2 (non-editor) casts: accepted, but escalation is signalled (pool = 1 < 2).
        let p = host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();
        assert!(p.escalation_required);

        // op1 (the editor) is a maker in strict mode -> rejected.
        let err = host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap_err();
        assert_eq!(err, GateHostError::Cast(kontur_core::CastRejected::Ineligible));
    }

    #[tokio::test]
    async fn gate_view_carries_files_and_loc() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (_gid, _dh) = open_a_gate(&host, &ws, &context).await;
        let view = &host.pending_gates().await[0];
        assert_eq!(view.files, vec!["a.rs".to_string()]);
        assert_eq!(view.loc, 1);
    }

    #[tokio::test]
    async fn gate_diff_and_audit_len() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        let diff = host.gate_diff(&gid).await.expect("diff bytes");
        assert!(!diff.is_empty());
        assert_eq!(host.audit_len().await, 0);

        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();
        assert_eq!(host.audit_len().await, 1);
    }

    #[tokio::test]
    async fn merge_session_after_satisfied_gate_records_message() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let (gid, dh) = open_a_gate(&host, &ws, &context).await;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();

        host.merge_session("m").await.unwrap();
        assert_eq!(ws.merged_message(), Some("m".to_string()));
    }

    #[tokio::test]
    async fn blocked_then_reworked_satisfied_chains_two_records() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let (gid, dh) = open_a_gate(&host, &ws, &context).await;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, nogo_verdict(2, &gid, dh, "cache it")).await.unwrap();
        assert_eq!(host.audit_len().await, 1);

        // Rework: new write, fresh gate, both go.
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"reworked\n").unwrap();
        let (gid2, _rx) = host.begin_task_gate(task, 0).await.unwrap();
        let dh2 = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid2, go_verdict(1, &gid2, dh2)).await.unwrap();
        host.submit_verdict(&gid2, go_verdict(2, &gid2, dh2)).await.unwrap();

        assert_eq!(host.audit_len().await, 2);
        assert!(host.verify_audit().await.is_ok());
    }

    #[tokio::test]
    async fn abandon_session_discards_pending_and_emits_event() {
        use tokio::sync::broadcast::error::TryRecvError;
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Open a gate (task in Open/Partial state).
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        // Cast one key → Partial state.
        let p = host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        assert_eq!(p.state, HoldState::Partial);

        let mut ev_rx = host.subscribe_events();

        // Abandon: discards the pending task, emits SessionAbandoned.
        host.abandon_session().await.unwrap();

        // The pending task was discarded.
        assert_eq!(ws.discarded_tasks(), vec![kontur_core::TaskId("t1".into())]);
        // Nothing was accepted.
        assert!(ws.accepted_tasks().is_empty());
        // The audit chain is still valid (no records for this partial gate, so chain is empty-valid).
        assert!(host.verify_audit().await.is_ok());

        // SessionAbandoned event must have been emitted.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut got_abandoned = false;
        loop {
            match ev_rx.try_recv() {
                Ok(HostEvent::SessionAbandoned) => { got_abandoned = true; break; }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline { break; }
                    tokio::task::yield_now().await;
                }
                _ => break,
            }
        }
        assert!(got_abandoned, "SessionAbandoned event must be emitted");
    }

    #[tokio::test]
    async fn abandon_session_skips_satisfied_tasks() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Open and fully satisfy a gate (task becomes Satisfied/accepted).
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();

        assert_eq!(ws.accepted_tasks(), vec![kontur_core::TaskId("t1".into())]);
        assert_eq!(host.audit_len().await, 1);

        // Abandon: no pending tasks to discard, chain stays intact.
        host.abandon_session().await.unwrap();

        // No discards from abandon (already accepted earlier).
        assert!(ws.discarded_tasks().is_empty());
        assert!(host.verify_audit().await.is_ok());
    }

    #[tokio::test]
    async fn propose_plan_emits_event_and_approve_flips_watch() {
        use std::time::Duration;
        use tokio::sync::broadcast::error::TryRecvError;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        let mut ev_rx = host.subscribe_events();

        let tasks = vec!["add caching".to_string(), "write tests".to_string()];
        let mut rx = host.propose_plan(tasks.clone()).await.unwrap();

        // proposed_plan() returns the tasks.
        assert_eq!(host.proposed_plan().await, Some(tasks.clone()));

        // The watch starts false.
        assert!(!*rx.borrow());

        // A PlanProposed event was emitted.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut got_event = false;
        loop {
            match ev_rx.try_recv() {
                Ok(HostEvent::PlanProposed { tasks: t }) => {
                    assert_eq!(t, tasks);
                    got_event = true;
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                _ => break,
            }
        }
        assert!(got_event, "PlanProposed event not received");

        // Obtain a fresh receiver on the same proposal channel.
        let mut rx2 = {
            let st = host.state.lock().await;
            st.plan_approved_tx.subscribe()
        };
        assert!(!*rx2.borrow());

        // Approve.
        host.approve_plan().await;

        // Both receivers on the current channel observe true after approval.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2.changed()).await;
        assert!(*rx.borrow_and_update(), "rx must see true after approve");
        assert!(*rx2.borrow_and_update(), "rx2 (direct subscribe) must see true");

        // A receiver obtained AFTER approve on the same channel sees true immediately (send_replace property).
        let rx3 = {
            let st = host.state.lock().await;
            st.plan_approved_tx.subscribe()
        };
        assert!(*rx3.borrow(), "subscriber after send_replace(true) must see true immediately");
    }

    #[tokio::test]
    async fn propose_plan_idempotent_overwrite() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        host.propose_plan(vec!["task-a".to_string()]).await.unwrap();
        host.propose_plan(vec!["task-b".to_string(), "task-c".to_string()]).await.unwrap();

        // Re-proposal overwrites.
        assert_eq!(
            host.proposed_plan().await,
            Some(vec!["task-b".to_string(), "task-c".to_string()])
        );
    }

    #[tokio::test]
    async fn reproposal_resets_approval() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        // Propose plan A and approve it.
        let mut rx1 = host.propose_plan(vec!["task-a".to_string()]).await.unwrap();
        host.approve_plan().await;

        // The original receiver sees true.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx1.changed()).await;
        assert!(*rx1.borrow_and_update(), "original rx must see true after approve");

        // Re-propose plan B: fresh watch channel, state reset to false.
        let mut rx2 = host.propose_plan(vec!["task-b".to_string()]).await.unwrap();
        assert!(!*rx2.borrow(), "re-proposal must reset approval to false");

        // The old receiver's changed() now errors (channel closed by the swap).
        let changed_result = tokio::time::timeout(Duration::from_millis(100), rx1.changed()).await;
        assert!(
            changed_result.is_err() || matches!(changed_result, Ok(Err(_))),
            "old receiver's changed() must fail after channel swap"
        );

        // Approve the new plan: fresh watch transitions to true.
        host.approve_plan().await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2.changed()).await;
        assert!(*rx2.borrow_and_update(), "new rx must see true after fresh approve");
    }

    #[tokio::test]
    async fn event_stream_write_gate_opened_gate_resolved() {
        use std::time::Duration;
        use tokio::sync::broadcast::error::TryRecvError;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let mut rx = host.subscribe_events();

        // Record a write → Write event.
        let task = TaskId("t1".into());
        host.record_write(&task, "a.rs", b"hello\n").await.unwrap();

        // Open a gate via begin_task_gate → GateOpened event.
        let (gid, _watch_rx) = host.begin_task_gate(task.clone(), 10).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // Cast two go verdicts → GateResolved{Satisfied} event.
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();

        // Collect all events with a short timeout so we don't wait forever.
        let mut events: Vec<HostEvent> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        loop {
            match rx.try_recv() {
                Ok(ev) => events.push(ev),
                Err(TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => break,
            }
        }

        // Assert the sequence contains: Write → GateOpened → GateResolved{Satisfied}
        let has_write = events.iter().any(|e| matches!(e, HostEvent::Write { path, .. } if path == "a.rs"));
        let has_gate_opened = events.iter().any(|e| matches!(e, HostEvent::GateOpened { .. }));
        let has_resolved = events.iter().any(|e| matches!(e, HostEvent::GateResolved { state: HoldState::Satisfied, .. }));

        assert!(has_write, "expected Write event; got: {events:?}");
        assert!(has_gate_opened, "expected GateOpened event; got: {events:?}");
        assert!(has_resolved, "expected GateResolved(Satisfied) event; got: {events:?}");

        // Verify ordering: Write before GateOpened before GateResolved.
        let write_pos = events.iter().position(|e| matches!(e, HostEvent::Write { .. })).unwrap();
        let opened_pos = events.iter().position(|e| matches!(e, HostEvent::GateOpened { .. })).unwrap();
        let resolved_pos = events.iter().position(|e| matches!(e, HostEvent::GateResolved { state: HoldState::Satisfied, .. })).unwrap();
        assert!(write_pos < opened_pos, "Write must precede GateOpened");
        assert!(opened_pos < resolved_pos, "GateOpened must precede GateResolved");
    }

    // -----------------------------------------------------------------------
    // T5 race-fix tests
    // -----------------------------------------------------------------------

    /// After `abandon_session`, a second `submit_verdict` must be refused with
    /// `SessionAbandoned`; the audit chain must be unchanged from before abandon,
    /// and the task must have been discarded.
    #[tokio::test]
    async fn casts_refused_after_abandon() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        // First go cast — gate moves to Partial.
        let p1 = host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        assert_eq!(p1.state, HoldState::Partial);

        let audit_before = host.audit_len().await;

        // Abandon — discards the task.
        host.abandon_session().await.unwrap();
        assert_eq!(ws.discarded_tasks(), vec![TaskId("t1".into())]);

        // Second go cast after abandon must fail.
        let err = host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap_err();
        assert_eq!(err, GateHostError::SessionAbandoned);

        // Audit chain must be unchanged (no records added after abandon).
        assert_eq!(host.audit_len().await, audit_before);
    }

    /// After `abandon_session`, calling `begin_task_gate` must return
    /// `Err(GateHostError::SessionAbandoned)`.
    #[tokio::test]
    async fn begin_task_gate_refused_after_abandon() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        host.abandon_session().await.unwrap();

        // Seed a task write so the workspace can freeze it.
        let task = TaskId("t2".into());
        ws.apply_write(&task, "b.rs", b"y\n").unwrap();

        let err = host.begin_task_gate(task, 10).await.unwrap_err();
        assert_eq!(err, GateHostError::SessionAbandoned);
    }

    /// After `abandon_session`, calling `hand_edit` must return
    /// `Err(GateHostError::SessionAbandoned)`.
    #[tokio::test]
    async fn hand_edit_refused_after_abandon() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws.clone());

        host.abandon_session().await.unwrap();

        let task = TaskId("t1".into());
        let err = host.hand_edit(task, "a.rs", b"x\n", op1).await.unwrap_err();
        assert_eq!(err, GateHostError::SessionAbandoned);
    }

    /// After `abandon_session`, calling `propose_plan` must return
    /// `Err(GateHostError::SessionAbandoned)`.
    /// set_prompt updates the session context so that subsequent gate provenance
    /// (built from ctx.prompt) carries the new text. We verify by opening a gate
    /// after the update and inspecting the satisfied audit record.
    #[tokio::test]
    async fn set_prompt_updates_provenance() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Update the prompt before the gate opens.
        host.set_prompt("updated in-console".to_string()).await;

        // Verify the context was updated.
        assert_eq!(host.state.lock().await.ctx.prompt, "updated in-console");

        // Open a gate and satisfy it so provenance is captured in the audit record.
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (gid, _rx) = host.begin_task_gate(task, 0).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh)).await.unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh)).await.unwrap();

        // The satisfied gate's audit record must carry the updated prompt.
        let st = host.state.lock().await;
        let record = st.chain.records().iter().find(|r| r.core.gate_id == gid).unwrap();
        assert_eq!(
            record.core.provenance.prompt, "updated in-console",
            "audit record must carry the in-console-edited prompt"
        );
    }

    #[tokio::test]
    async fn propose_plan_refused_after_abandon() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        host.abandon_session().await.unwrap();

        let err = host.propose_plan(vec!["task-a".to_string()]).await.unwrap_err();
        assert_eq!(err, GateHostError::SessionAbandoned);
    }
}
