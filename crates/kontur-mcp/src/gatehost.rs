use std::sync::Arc;

use kontur_core::{
    reviewed_by as core_reviewed_by, verify_chain, AuditChain, Authorship, CastVerdict, ChainBreak,
    DualHold, GateId, GateRecord, Hash, HoldState, MakerSet, OperatorId, Provenance, Remedy,
    TaskId, VerdictView,
};
use tokio::sync::{broadcast, watch, Mutex};

use crate::error::GateHostError;
use crate::provenance::build_provenance;
use crate::session::SessionContext;
use crate::workspace::{diff_hash, CommandOutput, Workspace};

/// Decision state for the plan-approval watch channel.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanDecision {
    Pending,
    Approved,
    Steered(String),
}

/// A clarification question the agent asks the operators. The "provide your own
/// answer" option is implicit and offered by the console, not stored here.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClarifyQuestion {
    pub prompt: String,
    pub options: Vec<String>,
}

/// Resolution of a clarification exchange, awaited by the parked MCP call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClarifyDecision {
    Pending,
    /// One or more accepted answers per original question, in order.
    Answered(Vec<Vec<String>>),
}

/// Live activity events for observers (the session server). Best-effort
/// display stream — never blocks or gates the enforcement path.
#[derive(Clone, Debug)]
pub enum HostEvent {
    Write {
        agent: String,
        task: TaskId,
        path: String,
        bytes: usize,
    },
    Command {
        agent: String,
        task: TaskId,
        command: String,
        /// Exit code of the completed command — the event is emitted after
        /// execution so reviewers see outcomes, not just invocations.
        exit_code: i32,
    },
    GateOpened {
        agent: String,
        gate_id: GateId,
        task: TaskId,
    },
    GateResolved {
        gate_id: GateId,
        state: HoldState,
    },
    /// Emitted after `hand_edit` removes a stale pending hold and replaces it
    /// with a fresh one over the combined diff. The wire then projects the
    /// fresh gate — realtime property.
    GateSuperseded {
        old_gate_id: GateId,
        new_gate_id: GateId,
    },
    PlanProposed {
        agent: String,
        tasks: Vec<String>,
    },
    /// Emitted after `steer_plan` routes a replan prompt to the agent.
    PlanSteered {
        steer: String,
    },
    /// The agent asked the operators to clarify ambiguity before planning.
    QuestionsAsked {
        agent: String,
        questions: Vec<ClarifyQuestion>,
    },
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
    /// The agent that produced this gate's change.
    pub agent: String,
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
    /// Senders from superseded gates that this entry now speaks for.
    ///
    /// When a hand-edit supersedes a pending gate, the old gate's watch sender
    /// is transferred here rather than fired with a fake `Satisfied`. When
    /// `submit_verdict` resolves this hold (either Satisfied or Blocked), it
    /// publishes the real outcome on `watch_tx` AND on every sender here, so
    /// agents parked on old receivers wake with the true combined-gate result.
    ///
    /// On double-supersede (hand-edit on a hand-edit), the intermediate entry's
    /// `carried_watchers` are appended into the newest entry, so the original
    /// agent's receiver is always reachable.
    carried_watchers: Vec<watch::Sender<HoldState>>,
}

struct SessionState {
    ctx: SessionContext,
    chain: AuditChain,
    holds: Vec<HoldEntry>,
    next_gate: u64,
    plan: Option<Vec<String>>,
    questions: Option<Vec<ClarifyQuestion>>,
    clarify_decision_tx: watch::Sender<ClarifyDecision>,
    _clarify_decision_rx: watch::Receiver<ClarifyDecision>,
    plan_decision_tx: watch::Sender<PlanDecision>,
    /// Kept alive so `send_replace` on `plan_decision_tx` is never a no-op
    /// (watch::send discards when there are zero receivers; keeping one here
    /// guarantees the channel is always live — same pattern as kontur-net).
    _plan_decision_rx: watch::Receiver<PlanDecision>,
    /// Set by `abandon_session` under the state lock. Once `true`,
    /// `submit_verdict`, `begin_task_gate`, `hand_edit`, and `propose_plan`
    /// all return `Err(GateHostError::SessionAbandoned)` immediately.
    ///
    /// Coherence: a cast that beats the flag commits before discard → discard
    /// resets to the new HEAD (harmless, audit coherent). A cast after the
    /// flag is refused → no accept post-abandon.
    abandoned: bool,
    /// Supersession redirect table: `(old_gate_id, new_gate_id)`.
    ///
    /// When `hand_edit` supersedes a pending gate, a mapping from the old id
    /// to the fresh gate's id is recorded here. `gate_outcome` follows the
    /// chain transitively (a hand-edit on a hand-edit chains) so an agent
    /// querying ANY earlier gate id gets the terminal outcome of whichever
    /// live gate now owns that task's diff. The table is append-only and
    /// never mutated once written — old entries remain to support queries for
    /// any point in the supersession chain.
    superseded: Vec<(GateId, GateId)>,
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
        let (plan_decision_tx, _plan_decision_rx) = watch::channel(PlanDecision::Pending);
        let (clarify_decision_tx, _clarify_decision_rx) = watch::channel(ClarifyDecision::Pending);
        GateHost {
            state: Arc::new(Mutex::new(SessionState {
                ctx,
                chain: AuditChain::new(),
                holds: Vec::new(),
                next_gate: 0,
                plan: None,
                questions: None,
                clarify_decision_tx,
                _clarify_decision_rx,
                plan_decision_tx,
                _plan_decision_rx,
                abandoned: false,
                superseded: Vec::new(),
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
    /// to `PlanDecision::Approved` when both operators approve (or
    /// `PlanDecision::Steered` when they route a replan). Re-proposal overwrites
    /// (idempotent).
    pub async fn propose_plan(
        &self,
        agent: &str,
        tasks: Vec<String>,
    ) -> Result<watch::Receiver<PlanDecision>, GateHostError> {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
        // BUG CLASS: approval state lifetime must match proposal, not session.
        // Create a fresh watch channel on each proposal. Prior subscribers are
        // closed (their changed() errors), correctly surfacing "plan superseded".
        // This prevents the stale-approval bypass: after approve_plan() sets
        // `Approved`, a re-proposal returning a subscriber from the *old* channel
        // would immediately read Approved without operator action.
        let (new_tx, new_rx) = watch::channel(PlanDecision::Pending);
        st.plan_decision_tx = new_tx;
        st._plan_decision_rx = new_rx;

        st.plan = Some(tasks.clone());
        // Return a new subscriber BEFORE releasing the lock so the initial
        // `Pending` is always visible and `send_replace(Approved)` from
        // approve_plan cannot race past this subscribe.
        let rx = st.plan_decision_tx.subscribe();
        drop(st);
        let _ = self.events.send(HostEvent::PlanProposed {
            agent: agent.to_owned(),
            tasks,
        });
        Ok(rx)
    }

    /// Operator face: mark the proposed plan as approved, unblocking any
    /// awaiter on the watch returned by `propose_plan`.
    pub async fn approve_plan(&self) {
        let st = self.state.lock().await;
        // send_replace never discards (we keep _plan_decision_rx alive in state).
        st.plan_decision_tx.send_replace(PlanDecision::Approved);
    }

    /// Operator face: route a steer prompt to the agent to revise its plan.
    /// Resolves the watch returned by `propose_plan` with `PlanDecision::Steered`
    /// so the parked MCP call returns an error carrying the steer. No-op when the
    /// session has been abandoned — same silent pattern as `set_plan`.
    pub async fn steer_plan(&self, steer: String) {
        {
            let st = self.state.lock().await;
            if st.abandoned {
                return;
            }
            st.plan_decision_tx
                .send_replace(PlanDecision::Steered(steer.clone()));
        }
        let _ = self.events.send(HostEvent::PlanSteered { steer });
    }

    /// Operator face: read the currently proposed plan (None until one arrives).
    pub async fn proposed_plan(&self) -> Option<Vec<String>> {
        self.state.lock().await.plan.clone()
    }

    /// Agent face: ask the operators to clarify ambiguity. Stores the questions,
    /// emits `HostEvent::QuestionsAsked`, and returns a watch receiver that flips
    /// to `Answered` once the operators resolve the exchange. Same fresh-channel
    /// discipline as `propose_plan` so a stale resolution can't bypass consent.
    pub async fn ask_clarification(
        &self,
        agent: &str,
        questions: Vec<ClarifyQuestion>,
    ) -> Result<watch::Receiver<ClarifyDecision>, GateHostError> {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
        let (new_tx, new_rx) = watch::channel(ClarifyDecision::Pending);
        st.clarify_decision_tx = new_tx;
        st._clarify_decision_rx = new_rx;
        st.questions = Some(questions.clone());
        let rx = st.clarify_decision_tx.subscribe();
        drop(st);
        let _ = self.events.send(HostEvent::QuestionsAsked {
            agent: agent.to_owned(),
            questions,
        });
        Ok(rx)
    }

    /// Operator face: resolve the clarification exchange with the accepted
    /// answers, unblocking the parked `ask_clarification` MCP call.
    pub async fn resolve_clarification(&self, answers: Vec<Vec<String>>) {
        let st = self.state.lock().await;
        st.clarify_decision_tx
            .send_replace(ClarifyDecision::Answered(answers));
    }

    /// Operator face: the questions currently awaiting answers (None until asked).
    pub async fn asked_questions(&self) -> Option<Vec<ClarifyQuestion>> {
        self.state.lock().await.questions.clone()
    }

    /// Operator face: replace the stored plan with an edited version.
    /// Called when an operator edits the plan in-console during PlanReview.
    /// No-op when the session has been abandoned — avoids races with abandon.
    pub async fn set_plan(&self, tasks: Vec<String>) {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return;
        }
        st.plan = Some(tasks);
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
    pub async fn record_write(
        &self,
        agent: &str,
        task_id: &TaskId,
        path: &str,
        contents: &[u8],
    ) -> Result<(), GateHostError> {
        self.workspace.apply_write(task_id, path, contents)?;
        let _ = self.events.send(HostEvent::Write {
            agent: agent.to_owned(),
            task: task_id.clone(),
            path: path.to_owned(),
            bytes: contents.len(),
        });
        Ok(())
    }

    /// Agent face: run a command in the worktree (not gated).
    pub async fn run_command(
        &self,
        agent: &str,
        task_id: &TaskId,
        command: &str,
        cwd: &str,
    ) -> Result<CommandOutput, GateHostError> {
        let out = self.workspace.run_command(task_id, command, cwd)?;
        let _ = self.events.send(HostEvent::Command {
            agent: agent.to_owned(),
            task: task_id.clone(),
            command: command.to_owned(),
            exit_code: out.exit_code,
        });
        Ok(out)
    }

    /// Open a gate over a task's frozen diff. Returns the gate id and a receiver
    /// the awaiting agent-side handler watches for resolution.
    pub async fn open_gate(
        &self,
        agent: &str,
        task_id: TaskId,
        provenance: Provenance,
    ) -> (GateId, watch::Receiver<HoldState>) {
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
        st.holds.push(HoldEntry {
            hold,
            provenance,
            watch_tx: tx,
            escalation_required: false,
            carried_watchers: vec![],
        });
        drop(st);
        let _ = self.events.send(HostEvent::GateOpened {
            agent: agent.to_owned(),
            gate_id: id.clone(),
            task: task_id_for_event,
        });
        (id, rx)
    }

    /// Operator face: cast a signed verdict on a gate. On resolution, accepts or
    /// discards the task and publishes the new state on the gate's watch.
    pub async fn submit_verdict(
        &self,
        gate_id: &GateId,
        cv: CastVerdict,
    ) -> Result<GateProgress, GateHostError> {
        let mut st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
        let idx = st
            .holds
            .iter()
            .position(|e| e.hold.gate_id() == gate_id)
            .ok_or_else(|| GateHostError::UnknownGate(gate_id.0.clone()))?;

        // Enforce the session roster at the gate boundary: only registered
        // operators (the two seat keys — for BYO seat B, the host-approved key)
        // may satisfy a gate. Closes the case where an unregistered key reaches
        // the engine; the sentinel is never in the roster, so it is refused.
        if !st.ctx.operators.contains(&cv.operator) {
            return Err(GateHostError::Cast(kontur_core::CastRejected::Ineligible));
        }
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
                st.chain
                    .append(record)
                    .expect("chain head matches prev by construction");
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
                st.chain
                    .append(record)
                    .expect("chain head matches prev by construction");
                self.workspace.discard_task(&task_id)?;
                remedy
            }
            _ => None,
        };

        let escalation_required = st.holds[idx].escalation_required;
        // Publish the new state on the primary watch channel.
        let _ = st.holds[idx].watch_tx.send(state);
        // Also publish on every carried sender — these belong to agents that
        // were parked on a superseded gate's receiver and were transferred here
        // by `hand_edit` instead of being given a fake Satisfied signal. They
        // now receive the true combined-gate outcome.
        for tx in &st.holds[idx].carried_watchers {
            let _ = tx.send(state);
        }
        let gate_id_for_event = gate_id.clone();
        drop(st);
        if matches!(state, HoldState::Satisfied | HoldState::Blocked) {
            let _ = self.events.send(HostEvent::GateResolved {
                gate_id: gate_id_for_event,
                state,
            });
        }
        Ok(GateProgress {
            state,
            escalation_required,
            remedy,
        })
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
        agent: &str,
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
            let mut p = build_provenance(&st.ctx, &task_id, dh, &frozen, tokens);
            // Attribute the gate to the agent that proposed it (per-agent in a
            // fleet; the session default otherwise).
            p.agent_id = agent.to_owned();
            p
        };
        Ok(self.open_gate(agent, task_id, provenance).await)
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
                agent: e.provenance.agent_id.clone(),
            })
            .collect()
    }

    /// Read a gate's terminal outcome (for the awaiting agent handler).
    /// Returns Some for a gate in ANY state; callers must inspect `state` before acting.
    ///
    /// # Supersession redirect
    ///
    /// When a `hand_edit` supersedes a pending gate it records a mapping
    /// `(old_gate_id → new_gate_id)` in `SessionState::superseded`. This
    /// method follows those mappings transitively before lookup, so an agent
    /// querying an old gate id (because it called `begin_task_gate` before the
    /// hand-edit arrived) gets the terminal outcome of the live gate that now
    /// owns the combined diff. Chains are followed to completion — a hand-edit
    /// on a hand-edit is handled correctly because each step appends its own
    /// redirect.
    pub async fn gate_outcome(&self, gate_id: &GateId) -> Option<GateFinal> {
        let st = self.state.lock().await;

        // Follow supersession chain transitively.
        let mut effective_id = gate_id;
        // Use a step counter to guard against any hypothetical cycle (the
        // table is append-only so cycles cannot form, but we bound the walk
        // defensively).
        let mut steps = 0usize;
        while let Some((_, new_id)) = st.superseded.iter().find(|(old, _)| old == effective_id) {
            effective_id = new_id;
            steps += 1;
            // Safety bound: more redirects than entries is impossible in a
            // well-formed table. Break and fall through to a not-found result
            // rather than looping forever.
            if steps > st.superseded.len() {
                break;
            }
        }

        let e = st.holds.iter().find(|e| e.hold.gate_id() == effective_id)?;
        let state = e.hold.state();
        let remedy = e.hold.blocking_remedy();
        let reviewed_by = st
            .chain
            .records()
            .iter()
            .find(|r| &r.core.gate_id == effective_id)
            .map(core_reviewed_by)
            .unwrap_or_default();
        Some(GateFinal {
            state,
            remedy,
            reviewed_by,
        })
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
        let handedit_agent = provenance.agent_id.clone();
        let hold = DualHold::reopen_handedit(
            id.clone(),
            task_id.clone(),
            dh,
            st.ctx.policy,
            MakerSet::new(),
            editor,
            true,
            &st.ctx.operators,
        );
        let (tx, rx) = watch::channel(hold.state());
        let escalation_required = hold.escalation_required();

        // Within the same lock: remove all prior Open/Partial holds for this
        // task_id. Resolved holds (Satisfied/Blocked) are kept — their audit
        // records stand. Verdicts on a superseded hold must return UnknownGate
        // ("superseded by hand-edit; verdicts must bind the combined diff").
        //
        // CORRECTNESS: do NOT send any signal on the superseded holds' watch
        // channels here. Sending `Satisfied` would tell a parked
        // `propose_task_complete` that the task was APPROVED before the
        // combined diff has been reviewed. If the fresh gate is then no-go'd,
        // the agent has already moved on — workflow integrity broken.
        //
        // Instead we TRANSFER: the new `HoldEntry` collects the superseded
        // entries' watch senders in `carried_watchers`. When `submit_verdict`
        // resolves the fresh gate it publishes the real outcome on every
        // carried sender, so the parked agent wakes with the true result.
        //
        // On double-supersede (hand-edit on a hand-edit), the intermediate
        // entry's `carried_watchers` are appended too — every generation's
        // receivers end up in the newest entry.

        // Phase 1: partition holds into superseded vs. kept. Collect the
        // senders from superseded entries before dropping their HoldEntry
        // values (moving out of the vec before retain removes the items).
        let mut kept_holds: Vec<HoldEntry> = Vec::new();
        let mut carried_watchers: Vec<watch::Sender<HoldState>> = Vec::new();
        let mut superseded_ids: Vec<GateId> = Vec::new();
        for entry in st.holds.drain(..) {
            if entry.hold.task_id() == &task_id
                && matches!(entry.hold.state(), HoldState::Open | HoldState::Partial)
            {
                superseded_ids.push(entry.hold.gate_id().clone());
                // Transfer the primary sender and any already-carried senders
                // into the new entry's carry list.
                carried_watchers.push(entry.watch_tx);
                carried_watchers.extend(entry.carried_watchers);
            } else {
                kept_holds.push(entry);
            }
        }
        st.holds = kept_holds;

        // Record supersession redirects so `gate_outcome` can follow chains.
        for old_id in &superseded_ids {
            st.superseded.push((old_id.clone(), id.clone()));
        }

        st.holds.push(HoldEntry {
            hold,
            provenance,
            watch_tx: tx,
            escalation_required,
            carried_watchers,
        });
        drop(st);

        // Emit supersession events after the lock drops (best-effort display).
        for old_id in &superseded_ids {
            let _ = self.events.send(HostEvent::GateSuperseded {
                old_gate_id: old_id.clone(),
                new_gate_id: id.clone(),
            });
        }
        let _ = self.events.send(HostEvent::GateOpened {
            agent: handedit_agent,
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
        let task_id = st
            .holds
            .iter()
            .find(|e| e.hold.gate_id() == gate_id)?
            .hold
            .task_id()
            .clone();
        drop(st);
        self.workspace
            .freeze_task_diff(&task_id)
            .ok()
            .map(|f| f.bytes)
    }

    /// Number of records currently in the audit chain.
    pub async fn audit_len(&self) -> usize {
        self.state.lock().await.chain.records().len()
    }

    /// The chain head hash (GENESIS when no gate has resolved).
    pub async fn audit_head(&self) -> kontur_core::Hash {
        self.state.lock().await.chain.head()
    }

    /// Persist the audit chain to the workspace's audit dir as JSON.
    ///
    /// Returns `None` when the workspace has no durable home (in-memory
    /// sessions) or the chain is empty; otherwise the write result. The file
    /// is named after the chain head, so it is content-addressed and a
    /// re-write of the same chain is idempotent.
    pub async fn persist_audit(&self) -> Option<std::io::Result<std::path::PathBuf>> {
        let dir = self.workspace.audit_dir()?;
        let (records, head) = {
            let st = self.state.lock().await;
            (st.chain.records().to_vec(), st.chain.head())
        };
        if records.is_empty() {
            return None;
        }
        let head_hex: String = head.0[..8].iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("audit-{head_hex}.json"));
        let result = (|| {
            std::fs::create_dir_all(&dir)?;
            let json = serde_json::to_vec_pretty(&records)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            std::fs::write(&path, json)?;
            Ok(path)
        })();
        Some(result)
    }

    /// Read the current on-disk contents of a file in the task's worktree.
    /// Returns `Ok(None)` when the path does not exist (new file).
    /// Refused when the session has been abandoned — same guard as other
    /// mutating/reading operations.
    pub async fn read_file(
        &self,
        task_id: &TaskId,
        path: &str,
    ) -> Result<Option<Vec<u8>>, GateHostError> {
        let st = self.state.lock().await;
        if st.abandoned {
            return Err(GateHostError::SessionAbandoned);
        }
        drop(st);
        Ok(self.workspace.read_file(task_id, path)?)
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
            let mut tids = Vec::new();
            for e in st
                .holds
                .iter()
                .filter(|e| matches!(e.hold.state(), HoldState::Open | HoldState::Partial))
            {
                tids.push(e.hold.task_id().clone());
                // Parked proposals must not hang forever: Blocked is the honest
                // terminal for "will never be approved". Agents treat Blocked as
                // rejection; gate_outcome returns remedy None on Blocked, which
                // the ScriptedAgent rework path tolerates (None → "rework" string,
                // no unwrap, no panic).
                let _ = e.watch_tx.send(HoldState::Blocked);
                for tx in &e.carried_watchers {
                    let _ = tx.send(HoldState::Blocked);
                }
            }
            tids
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

    /// Register an operator into the session roster (idempotent). Called after
    /// the host approves a BYO seat-B key, so hand-edit eligibility and the
    /// Reviewed-by/audit roster include the real, approved operator rather than
    /// the placeholder configured at construction.
    pub async fn register_operator(&self, op: OperatorId) {
        let mut st = self.state.lock().await;
        if !st.ctx.operators.contains(&op) {
            st.ctx.operators.push(op);
        }
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

    /// Test double: an in-memory workspace that claims a durable audit home.
    struct AuditableWs {
        inner: InMemoryWorkspace,
        dir: std::path::PathBuf,
    }

    impl Workspace for AuditableWs {
        fn apply_write(
            &self,
            t: &TaskId,
            p: &str,
            c: &[u8],
        ) -> Result<(), crate::error::WorkspaceError> {
            self.inner.apply_write(t, p, c)
        }
        fn run_command(
            &self,
            t: &TaskId,
            c: &str,
            w: &str,
        ) -> Result<CommandOutput, crate::error::WorkspaceError> {
            self.inner.run_command(t, c, w)
        }
        fn freeze_task_diff(
            &self,
            t: &TaskId,
        ) -> Result<crate::workspace::FrozenDiff, crate::error::WorkspaceError> {
            self.inner.freeze_task_diff(t)
        }
        fn accept_task(&self, t: &TaskId) -> Result<(), crate::error::WorkspaceError> {
            self.inner.accept_task(t)
        }
        fn discard_task(&self, t: &TaskId) -> Result<(), crate::error::WorkspaceError> {
            self.inner.discard_task(t)
        }
        fn merge_session(&self, m: &str) -> Result<(), crate::error::WorkspaceError> {
            self.inner.merge_session(m)
        }
        fn read_file(
            &self,
            t: &TaskId,
            p: &str,
        ) -> Result<Option<Vec<u8>>, crate::error::WorkspaceError> {
            self.inner.read_file(t, p)
        }
        fn audit_dir(&self) -> Option<std::path::PathBuf> {
            Some(self.dir.clone())
        }
    }

    /// A gate is attributed to the agent that opened it (fleet attribution).
    #[tokio::test]
    async fn gate_carries_its_agent() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context, ws.clone());

        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (_gid, _rx) = host.begin_task_gate("agent-07", task, 0).await.unwrap();

        let pending = host.pending_gates().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].agent, "agent-07",
            "gate must carry its opening agent"
        );
    }

    /// persist_audit: a resolved gate round-trips through the JSON file and
    /// the reloaded chain verifies; the filename is content-addressed by the
    /// chain head. In-memory workspaces (no audit_dir) persist nothing.
    #[tokio::test]
    async fn persist_audit_roundtrip_and_verify() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let mut dir = std::env::temp_dir();
        dir.push(format!("kontur-audit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Arc::new(AuditableWs {
            inner: InMemoryWorkspace::new(),
            dir: dir.clone(),
        });
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Empty chain → nothing to persist.
        assert!(host.persist_audit().await.is_none());

        // Resolve one gate with two goes.
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let frozen = ws.freeze_task_diff(&task).unwrap();
        let dh = diff_hash(&frozen);
        let prov = build_provenance(&context, &task, dh, &frozen, 100);
        let (gid, _rx) = host.open_gate("agent-01", task, prov).await;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

        let path = host
            .persist_audit()
            .await
            .expect("workspace has an audit dir")
            .expect("write succeeds");
        let head = host.audit_head().await;
        let head_hex: String = head.0[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(&head_hex),
            "filename must be content-addressed by the chain head"
        );

        let bytes = std::fs::read(&path).unwrap();
        let records: Vec<kontur_core::GateRecord> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(records.len(), 1);
        kontur_core::verify_chain(&records).expect("reloaded chain must verify");
        // Tamper-evidence survives the round-trip: flip a byte, break the chain.
        let mut tampered = records.clone();
        tampered[0].core.provenance.loc += 1;
        assert!(kontur_core::verify_chain(&tampered).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In-memory workspace has no audit home: persist is a clean no-op.
    #[tokio::test]
    async fn persist_audit_none_for_memory_workspace() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();
        assert!(host.persist_audit().await.is_none());
    }

    /// run_command emits its event after execution, carrying the exit code.
    #[tokio::test]
    async fn command_event_carries_exit_code() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context, ws);
        let mut events = host.subscribe_events();
        host.run_command("agent-01", &TaskId("t1".into()), "cargo test", ".")
            .await
            .unwrap();
        match events.recv().await.unwrap() {
            HostEvent::Command {
                command, exit_code, ..
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(exit_code, 0);
            }
            other => panic!("expected Command event, got {other:?}"),
        }
    }

    /// A verdict from an operator not in the session roster is refused at the
    /// gate boundary (even with a valid signature over the right gate/diff).
    #[tokio::test]
    async fn unregistered_operator_verdict_is_refused() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        // Roster is only {op1, op2}. A third, unregistered signer (seed 7).
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        let stranger = go_verdict(7, &gid, dh); // seed 7 not in the roster
        let err = host.submit_verdict(&gid, stranger).await.unwrap_err();
        assert!(
            matches!(
                err,
                GateHostError::Cast(kontur_core::CastRejected::Ineligible)
            ),
            "unregistered operator must be refused; got {err:?}"
        );
        // A registered operator is still accepted.
        assert!(host
            .submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .is_ok());
    }

    fn go_verdict(seed: u8, gate_id: &GateId, dh: Hash) -> CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        CastVerdict::create(
            &signer,
            &FixedClock(1000 + seed as i64),
            gate_id,
            dh,
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }

    async fn open_a_gate(
        host: &GateHost,
        ws: &InMemoryWorkspace,
        ctx: &SessionContext,
    ) -> (GateId, Hash) {
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let frozen = ws.freeze_task_diff(&task).unwrap();
        let dh = diff_hash(&frozen);
        let prov = build_provenance(ctx, &task, dh, &frozen, 100);
        let (gid, _rx) = host.open_gate("agent-01", task, prov).await;
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

        let p1 = host
            .submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        assert_eq!(p1.state, HoldState::Partial);
        assert!(ws.accepted_tasks().is_empty());

        let p2 = host
            .submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();
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

        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        let p2 = host
            .submit_verdict(&gid, nogo_verdict(2, &gid, dh, "cache it"))
            .await
            .unwrap();

        assert_eq!(p2.state, HoldState::Blocked);
        assert_eq!(
            p2.remedy,
            Some(kontur_core::Remedy::Steer("cache it".into()))
        );
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

        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        let pending = host.pending_gates().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, HoldState::Partial);
        assert_eq!(pending[0].observed.len(), 1);
        assert_eq!(
            pending[0].observed[0].status,
            kontur_core::VerdictStatus::Sealed
        );
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
        let (gid, _rx) = host.begin_task_gate("agent-01", task, 42).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

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

        let (gid, _rx) = host
            .hand_edit(task.clone(), "a.rs", b"guarded\n", op1)
            .await
            .unwrap();
        // Applied immediately, observable in the workspace.
        assert_eq!(ws.file_contents(&task, "a.rs"), Some(b"guarded\n".to_vec()));

        let dh = host.pending_gates().await[0].diff_hash;
        // Editor op1 co-signs (pragmatic), op2 co-signs -> satisfied.
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        let p = host
            .submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();
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
        let (gid, _rx) = host
            .hand_edit(task, "a.rs", b"guarded\n", op1)
            .await
            .unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // op2 (non-editor) casts: accepted, but escalation is signalled (pool = 1 < 2).
        let p = host
            .submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();
        assert!(p.escalation_required);

        // op1 (the editor) is a maker in strict mode -> rejected.
        let err = host
            .submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            GateHostError::Cast(kontur_core::CastRejected::Ineligible)
        );
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

        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();
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
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

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
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, nogo_verdict(2, &gid, dh, "cache it"))
            .await
            .unwrap();
        assert_eq!(host.audit_len().await, 1);

        // Rework: new write, fresh gate, both go.
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"reworked\n").unwrap();
        let (gid2, _rx) = host.begin_task_gate("agent-01", task, 0).await.unwrap();
        let dh2 = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid2, go_verdict(1, &gid2, dh2))
            .await
            .unwrap();
        host.submit_verdict(&gid2, go_verdict(2, &gid2, dh2))
            .await
            .unwrap();

        assert_eq!(host.audit_len().await, 2);
        assert!(host.verify_audit().await.is_ok());
    }

    #[tokio::test]
    async fn abandon_session_discards_pending_and_emits_event() {
        use std::time::Duration;
        use tokio::sync::broadcast::error::TryRecvError;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Open a gate (task in Open/Partial state).
        let (gid, dh) = open_a_gate(&host, &ws, &context).await;

        // Cast one key → Partial state.
        let p = host
            .submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
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
                Ok(HostEvent::SessionAbandoned) => {
                    got_abandoned = true;
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
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

        assert_eq!(ws.accepted_tasks(), vec![kontur_core::TaskId("t1".into())]);
        assert_eq!(host.audit_len().await, 1);

        // Abandon: no pending tasks to discard, chain stays intact.
        host.abandon_session().await.unwrap();

        // No discards from abandon (already accepted earlier).
        assert!(ws.discarded_tasks().is_empty());
        assert!(host.verify_audit().await.is_ok());
    }

    #[tokio::test]
    async fn ask_clarification_blocks_then_resolve_flips_watch() {
        use std::time::Duration;
        use tokio::sync::broadcast::error::TryRecvError;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);
        let mut ev_rx = host.subscribe_events();

        let questions = vec![ClarifyQuestion {
            prompt: "target database?".into(),
            options: vec!["postgres".into(), "sqlite".into()],
        }];
        let mut rx = host
            .ask_clarification("agent-01", questions.clone())
            .await
            .unwrap();
        assert_eq!(host.asked_questions().await, Some(questions.clone()));
        assert_eq!(*rx.borrow(), ClarifyDecision::Pending);

        // A QuestionsAsked event was emitted.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut got = false;
        loop {
            match ev_rx.try_recv() {
                Ok(HostEvent::QuestionsAsked { questions: q, .. }) => {
                    assert_eq!(q, questions);
                    got = true;
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
        assert!(got, "QuestionsAsked event must be emitted");

        // Resolve unblocks the watch with the answers.
        host.resolve_clarification(vec![vec!["postgres".into()]])
            .await;
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            ClarifyDecision::Answered(vec![vec!["postgres".to_string()]])
        );
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
        let mut rx = host.propose_plan("agent-01", tasks.clone()).await.unwrap();

        // proposed_plan() returns the tasks.
        assert_eq!(host.proposed_plan().await, Some(tasks.clone()));

        // The watch starts Pending.
        assert_eq!(*rx.borrow(), PlanDecision::Pending);

        // A PlanProposed event was emitted.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut got_event = false;
        loop {
            match ev_rx.try_recv() {
                Ok(HostEvent::PlanProposed { tasks: t, .. }) => {
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
            st.plan_decision_tx.subscribe()
        };
        assert_eq!(*rx2.borrow(), PlanDecision::Pending);

        // Approve.
        host.approve_plan().await;

        // Both receivers on the current channel observe Approved after approval.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2.changed()).await;
        assert_eq!(
            *rx.borrow_and_update(),
            PlanDecision::Approved,
            "rx must see Approved after approve"
        );
        assert_eq!(
            *rx2.borrow_and_update(),
            PlanDecision::Approved,
            "rx2 (direct subscribe) must see Approved"
        );

        // A receiver obtained AFTER approve on the same channel sees Approved immediately (send_replace property).
        let rx3 = {
            let st = host.state.lock().await;
            st.plan_decision_tx.subscribe()
        };
        assert_eq!(
            *rx3.borrow(),
            PlanDecision::Approved,
            "subscriber after send_replace(Approved) must see Approved immediately"
        );
    }

    #[tokio::test]
    async fn propose_plan_idempotent_overwrite() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        host.propose_plan("agent-01", vec!["task-a".to_string()])
            .await
            .unwrap();
        host.propose_plan("agent-01", vec!["task-b".to_string(), "task-c".to_string()])
            .await
            .unwrap();

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
        let mut rx1 = host
            .propose_plan("agent-01", vec!["task-a".to_string()])
            .await
            .unwrap();
        host.approve_plan().await;

        // The original receiver sees Approved.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx1.changed()).await;
        assert_eq!(
            *rx1.borrow_and_update(),
            PlanDecision::Approved,
            "original rx must see Approved after approve"
        );

        // Re-propose plan B: fresh watch channel, state reset to Pending.
        let mut rx2 = host
            .propose_plan("agent-01", vec!["task-b".to_string()])
            .await
            .unwrap();
        assert_eq!(
            *rx2.borrow(),
            PlanDecision::Pending,
            "re-proposal must reset approval to Pending"
        );

        // The old receiver's changed() now errors (channel closed by the swap).
        let changed_result = tokio::time::timeout(Duration::from_millis(100), rx1.changed()).await;
        assert!(
            changed_result.is_err() || matches!(changed_result, Ok(Err(_))),
            "old receiver's changed() must fail after channel swap"
        );

        // Approve the new plan: fresh watch transitions to Approved.
        host.approve_plan().await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2.changed()).await;
        assert_eq!(
            *rx2.borrow_and_update(),
            PlanDecision::Approved,
            "new rx must see Approved after fresh approve"
        );
    }

    #[tokio::test]
    async fn steer_plan_resolves_watch_with_steered_and_emits_event() {
        use std::time::Duration;
        use tokio::sync::broadcast::error::TryRecvError;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        let mut ev_rx = host.subscribe_events();

        let mut rx = host
            .propose_plan("agent-01", vec!["task-a".to_string(), "task-b".to_string()])
            .await
            .unwrap();
        assert_eq!(*rx.borrow(), PlanDecision::Pending);

        // Steer routes a replan.
        host.steer_plan("split task 2".to_string()).await;

        let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        assert_eq!(
            *rx.borrow_and_update(),
            PlanDecision::Steered("split task 2".to_string()),
            "rx must observe Steered after steer_plan"
        );

        // A PlanSteered event was emitted.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut got_event = false;
        loop {
            match ev_rx.try_recv() {
                Ok(HostEvent::PlanSteered { steer }) => {
                    assert_eq!(steer, "split task 2");
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
        assert!(got_event, "PlanSteered event not received");
    }

    #[tokio::test]
    async fn reproposal_after_steer_resets_decision() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let host = GateHost::new(ctx(vec![op1, op2]), ws);

        // Propose plan A, steer it.
        let mut rx1 = host
            .propose_plan("agent-01", vec!["task-a".to_string()])
            .await
            .unwrap();
        host.steer_plan("rethink".to_string()).await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx1.changed()).await;
        assert_eq!(
            *rx1.borrow_and_update(),
            PlanDecision::Steered("rethink".to_string())
        );

        // Re-propose plan B: fresh channel starts at Pending.
        let mut rx2 = host
            .propose_plan("agent-01", vec!["task-b".to_string()])
            .await
            .unwrap();
        assert_eq!(
            *rx2.borrow(),
            PlanDecision::Pending,
            "re-proposal must reset to Pending"
        );

        // Approve the new plan: transitions to Approved.
        host.approve_plan().await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2.changed()).await;
        assert_eq!(
            *rx2.borrow_and_update(),
            PlanDecision::Approved,
            "new rx must see Approved after approve"
        );
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
        host.record_write("agent-01", &task, "a.rs", b"hello\n")
            .await
            .unwrap();

        // Open a gate via begin_task_gate → GateOpened event.
        let (gid, _watch_rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // Cast two go verdicts → GateResolved{Satisfied} event.
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

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
        let has_write = events
            .iter()
            .any(|e| matches!(e, HostEvent::Write { path, .. } if path == "a.rs"));
        let has_gate_opened = events
            .iter()
            .any(|e| matches!(e, HostEvent::GateOpened { .. }));
        let has_resolved = events.iter().any(|e| {
            matches!(
                e,
                HostEvent::GateResolved {
                    state: HoldState::Satisfied,
                    ..
                }
            )
        });

        assert!(has_write, "expected Write event; got: {events:?}");
        assert!(
            has_gate_opened,
            "expected GateOpened event; got: {events:?}"
        );
        assert!(
            has_resolved,
            "expected GateResolved(Satisfied) event; got: {events:?}"
        );

        // Verify ordering: Write before GateOpened before GateResolved.
        let write_pos = events
            .iter()
            .position(|e| matches!(e, HostEvent::Write { .. }))
            .unwrap();
        let opened_pos = events
            .iter()
            .position(|e| matches!(e, HostEvent::GateOpened { .. }))
            .unwrap();
        let resolved_pos = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    HostEvent::GateResolved {
                        state: HoldState::Satisfied,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(write_pos < opened_pos, "Write must precede GateOpened");
        assert!(
            opened_pos < resolved_pos,
            "GateOpened must precede GateResolved"
        );
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
        let p1 = host
            .submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        assert_eq!(p1.state, HoldState::Partial);

        let audit_before = host.audit_len().await;

        // Abandon — discards the task.
        host.abandon_session().await.unwrap();
        assert_eq!(ws.discarded_tasks(), vec![TaskId("t1".into())]);

        // Second go cast after abandon must fail.
        let err = host
            .submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap_err();
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

        let err = host
            .begin_task_gate("agent-01", task, 10)
            .await
            .unwrap_err();
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
        let (gid, _rx) = host.begin_task_gate("agent-01", task, 0).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid, go_verdict(1, &gid, dh))
            .await
            .unwrap();
        host.submit_verdict(&gid, go_verdict(2, &gid, dh))
            .await
            .unwrap();

        // The satisfied gate's audit record must carry the updated prompt.
        let st = host.state.lock().await;
        let record = st
            .chain
            .records()
            .iter()
            .find(|r| r.core.gate_id == gid)
            .unwrap();
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

        let err = host
            .propose_plan("agent-01", vec!["task-a".to_string()])
            .await
            .unwrap_err();
        assert_eq!(err, GateHostError::SessionAbandoned);
    }

    // -----------------------------------------------------------------------
    // abandon wakes parked agents
    // -----------------------------------------------------------------------

    /// After `abandon_session`, a watch receiver parked on an Open gate must
    /// immediately observe `Blocked` (not stay parked forever).
    #[tokio::test]
    async fn abandon_sends_blocked_to_parked_watchers() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let context = ctx(vec![op1, op2]);
        let host = GateHost::new(context.clone(), ws.clone());

        // Open a gate and park on its watch receiver.
        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (_gid, mut rx) = host.begin_task_gate("agent-01", task, 10).await.unwrap();

        // The receiver should currently see Open.
        assert_eq!(*rx.borrow(), HoldState::Open);

        // Abandon the session.
        host.abandon_session().await.unwrap();

        // The receiver must promptly observe Blocked.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        assert_eq!(
            *rx.borrow_and_update(),
            HoldState::Blocked,
            "parked watch must observe Blocked after abandon_session"
        );
    }

    /// Carried watchers (from superseded holds) also observe Blocked on abandon.
    #[tokio::test]
    async fn abandon_sends_blocked_to_carried_watchers() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        // Pragmatic so op1 can hand-edit without strict exclusion complications.
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        // Agent opens a gate and parks on rx.
        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (_orig_gid, mut orig_rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        // Hand-edit supersedes: orig_rx becomes a carried watcher on the new entry.
        host.hand_edit(task.clone(), "a.rs", b"human\n", op1)
            .await
            .unwrap();

        // Abandon — must wake the carried watcher too.
        host.abandon_session().await.unwrap();

        let _ = tokio::time::timeout(Duration::from_millis(100), orig_rx.changed()).await;
        assert_eq!(
            *orig_rx.borrow_and_update(),
            HoldState::Blocked,
            "carried watcher must observe Blocked after abandon_session"
        );
    }

    // -----------------------------------------------------------------------
    // hand_edit supersession tests
    // -----------------------------------------------------------------------

    /// `hand_edit` must remove all Open/Partial holds for the same task, leaving
    /// only the fresh gate in `pending_gates`. The task_id is preserved.
    #[tokio::test]
    async fn hand_edit_supersedes_stale_pending_hold() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        // Open the initial gate (simulates begin_task_gate after agent writes).
        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (original_gate_id, _rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        // Confirm one pending gate exists.
        assert_eq!(host.pending_gates().await.len(), 1);
        assert_eq!(host.pending_gates().await[0].gate_id, original_gate_id);

        // Hand-edit: must remove the stale gate and open a fresh one.
        let (fresh_gate_id, _rx2) = host
            .hand_edit(task.clone(), "a.rs", b"human\n", op1)
            .await
            .unwrap();

        // pending_gates shows ONLY the fresh gate; task_id is preserved.
        let pending = host.pending_gates().await;
        assert_eq!(pending.len(), 1, "only the fresh gate must remain pending");
        assert_eq!(
            pending[0].gate_id, fresh_gate_id,
            "fresh gate must be the only pending gate"
        );
        assert_ne!(
            fresh_gate_id, original_gate_id,
            "fresh gate must have a new id"
        );
        assert_eq!(pending[0].task_id, task, "task_id must be preserved");
    }

    /// After `hand_edit` supersedes the original gate, casting a verdict on the
    /// OLD gate id must return `UnknownGate`.
    #[tokio::test]
    async fn cast_on_superseded_gate_returns_unknown_gate() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        let host = GateHost::new(ctx(vec![op1, op2]), ws.clone());

        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (original_gate_id, _rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();
        let original_dh = host.pending_gates().await[0].diff_hash;

        // Supersede with a hand-edit.
        host.hand_edit(task.clone(), "a.rs", b"human\n", op1)
            .await
            .unwrap();

        // Cast on the now-superseded original gate → UnknownGate.
        // superseded by hand-edit; verdicts must bind the combined diff
        let err = host
            .submit_verdict(
                &original_gate_id,
                go_verdict(2, &original_gate_id, original_dh),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, GateHostError::UnknownGate(_)),
            "expected UnknownGate on superseded gate, got: {err:?}"
        );
    }

    /// A RESOLVED (Satisfied/Blocked) gate for the same task must NOT be
    /// removed by a subsequent `hand_edit`. Audit chain length is unchanged
    /// and still verifies.
    #[tokio::test]
    async fn hand_edit_does_not_remove_resolved_gate() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        let host = GateHost::new(ctx(vec![op1, op2]), ws.clone());

        // Open, satisfy (resolve) the first gate.
        ws.apply_write(&task, "a.rs", b"v1\n").unwrap();
        let (gid1, _rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();
        let dh1 = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid1, go_verdict(1, &gid1, dh1))
            .await
            .unwrap();
        host.submit_verdict(&gid1, go_verdict(2, &gid1, dh1))
            .await
            .unwrap();

        let audit_before = host.audit_len().await;
        assert_eq!(audit_before, 1, "one audit record for the satisfied gate");
        assert!(host.verify_audit().await.is_ok());

        // Now open a second gate for the same task (simulates rework) and then
        // hand-edit to supersede it. The resolved gate's record must survive.
        ws.apply_write(&task, "a.rs", b"v2\n").unwrap();
        host.begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        // Hand-edit supersedes the second (Open) gate. The first (Satisfied) gate
        // was already removed from holds at satisfaction time (it's in the chain,
        // not in holds). Audit chain must be unchanged.
        host.hand_edit(task.clone(), "a.rs", b"v3\n", op1)
            .await
            .unwrap();

        // Audit chain still has only the original resolved record — not corrupted.
        assert_eq!(
            host.audit_len().await,
            audit_before,
            "audit chain must not shrink"
        );
        assert!(
            host.verify_audit().await.is_ok(),
            "audit chain must still verify"
        );

        // The resolved gate is findable via reviewed_by (it's in the chain).
        assert!(
            host.reviewed_by(&gid1).await.is_some(),
            "resolved gate must still be auditable"
        );
    }

    // -----------------------------------------------------------------------
    // Carried-watcher correctness tests (fix for supersede-sends-Satisfied bug)
    // -----------------------------------------------------------------------

    /// When `hand_edit` supersedes a pending gate, the agent parked on the
    /// ORIGINAL rx must NOT observe `Satisfied` until the FRESH gate is
    /// actually resolved with two go verdicts.
    ///
    /// Sequence:
    ///   begin_task_gate (keep rx) → hand_edit same task → assert rx still
    ///   shows Open/Partial → cast two go verdicts on fresh gate → assert rx
    ///   now shows Satisfied.
    #[tokio::test]
    async fn superseded_agent_watch_gets_combined_outcome() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        // Pragmatic so op1 (the editor) may co-sign.
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        // Agent opens gate and parks on the watch receiver.
        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (original_gid, mut original_rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        // Operator hand-edits, superseding the original gate.
        let (fresh_gid, _fresh_rx) = host
            .hand_edit(task.clone(), "a.rs", b"human\n", op1)
            .await
            .unwrap();
        assert_ne!(fresh_gid, original_gid);

        // IMPORTANT: after the hand-edit but BEFORE any verdicts, the original
        // rx must still show Open or Partial — NOT Satisfied.
        let state_mid = *original_rx.borrow();
        assert!(
            matches!(state_mid, HoldState::Open | HoldState::Partial),
            "original rx must not observe Satisfied before fresh gate resolves; got: {state_mid:?}"
        );

        // Now resolve the fresh gate with two go verdicts.
        let fresh_dh = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&fresh_gid, go_verdict(1, &fresh_gid, fresh_dh))
            .await
            .unwrap();
        host.submit_verdict(&fresh_gid, go_verdict(2, &fresh_gid, fresh_dh))
            .await
            .unwrap();

        // The original rx must now observe Satisfied (the real combined outcome).
        let _ = tokio::time::timeout(Duration::from_millis(100), original_rx.changed()).await;
        let state_final = *original_rx.borrow_and_update();
        assert_eq!(
            state_final,
            HoldState::Satisfied,
            "original rx must observe Satisfied after fresh gate resolves"
        );

        // And the task must actually have been accepted.
        assert_eq!(ws.accepted_tasks(), vec![task]);
    }

    /// When the fresh gate after a hand-edit is no-go'd, the original rx must
    /// observe `Blocked` (not `Satisfied`). `gate_outcome` on the original gid
    /// must redirect to the fresh gate's remedy via the supersession chain.
    ///
    /// Uses pragmatic policy so op1 (the editor) remains eligible to co-sign,
    /// allowing a go from op2 followed by a no-go from op1 to resolve Blocked.
    #[tokio::test]
    async fn superseded_watch_gets_blocked_outcome() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        // Pragmatic: op1 (the editor) remains eligible to cast verdicts.
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (original_gid, mut original_rx) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        let (fresh_gid, _fresh_rx) = host
            .hand_edit(task.clone(), "a.rs", b"human\n", op1)
            .await
            .unwrap();

        // Resolve fresh gate as no-go: op2 go, then op1 no-go.
        let fresh_dh = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&fresh_gid, go_verdict(2, &fresh_gid, fresh_dh))
            .await
            .unwrap();
        host.submit_verdict(
            &fresh_gid,
            nogo_verdict(1, &fresh_gid, fresh_dh, "rework needed"),
        )
        .await
        .unwrap();

        // Original rx must observe Blocked.
        let _ = tokio::time::timeout(Duration::from_millis(100), original_rx.changed()).await;
        let state_final = *original_rx.borrow_and_update();
        assert_eq!(
            state_final,
            HoldState::Blocked,
            "original rx must observe Blocked when fresh gate is no-go'd"
        );

        // gate_outcome on original gid must redirect to the fresh gate's
        // terminal outcome (including the remedy).
        let outcome = host.gate_outcome(&original_gid).await.unwrap();
        assert_eq!(outcome.state, HoldState::Blocked);
        assert_eq!(
            outcome.remedy,
            Some(kontur_core::Remedy::Steer("rework needed".into())),
            "gate_outcome must follow redirect and return fresh gate's remedy"
        );
    }

    /// A double hand-edit (hand-edit on a hand-edit) must chain correctly:
    /// the original rx AND the first-hand-edit rx must both observe the final
    /// gate's outcome. `gate_outcome` must follow the redirect chain
    /// transitively.
    #[tokio::test]
    async fn double_supersede_chains() {
        use std::time::Duration;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let ws = Arc::new(InMemoryWorkspace::new());
        let task = TaskId("t1".into());
        // Pragmatic so both operators can co-sign regardless of edit history.
        let host = GateHost::new(
            ctx(vec![op1, op2]).with_policy(kontur_core::GatePolicy {
                independence: kontur_core::Independence::Pragmatic,
                ..kontur_core::GatePolicy::default()
            }),
            ws.clone(),
        );

        // Agent gate.
        ws.apply_write(&task, "a.rs", b"agent\n").unwrap();
        let (gid_orig, mut rx_orig) = host
            .begin_task_gate("agent-01", task.clone(), 10)
            .await
            .unwrap();

        // First hand-edit: supersedes agent gate.
        let (gid_mid, mut rx_mid) = host
            .hand_edit(task.clone(), "a.rs", b"human-v1\n", op1)
            .await
            .unwrap();

        // Second hand-edit: supersedes the first hand-edit gate.
        let (gid_final, _rx_final) = host
            .hand_edit(task.clone(), "a.rs", b"human-v2\n", op2)
            .await
            .unwrap();

        assert_ne!(gid_orig, gid_mid);
        assert_ne!(gid_mid, gid_final);

        // Before any verdicts: both rx_orig and rx_mid must still show non-Satisfied.
        let s_orig = *rx_orig.borrow();
        let s_mid = *rx_mid.borrow();
        assert!(
            matches!(s_orig, HoldState::Open | HoldState::Partial),
            "rx_orig must not prematurely show Satisfied; got: {s_orig:?}"
        );
        assert!(
            matches!(s_mid, HoldState::Open | HoldState::Partial),
            "rx_mid must not prematurely show Satisfied; got: {s_mid:?}"
        );

        // Resolve the final gate.
        let final_dh = host.pending_gates().await[0].diff_hash;
        host.submit_verdict(&gid_final, go_verdict(1, &gid_final, final_dh))
            .await
            .unwrap();
        host.submit_verdict(&gid_final, go_verdict(2, &gid_final, final_dh))
            .await
            .unwrap();

        // Both rx_orig and rx_mid must observe Satisfied.
        let _ = tokio::time::timeout(Duration::from_millis(100), rx_orig.changed()).await;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx_mid.changed()).await;
        assert_eq!(
            *rx_orig.borrow_and_update(),
            HoldState::Satisfied,
            "rx_orig must observe final gate's Satisfied"
        );
        assert_eq!(
            *rx_mid.borrow_and_update(),
            HoldState::Satisfied,
            "rx_mid must observe final gate's Satisfied"
        );

        // gate_outcome follows the redirect chain transitively.
        let outcome_orig = host.gate_outcome(&gid_orig).await.unwrap();
        assert_eq!(
            outcome_orig.state,
            HoldState::Satisfied,
            "gate_outcome(gid_orig) must redirect to final gate"
        );

        let outcome_mid = host.gate_outcome(&gid_mid).await.unwrap();
        assert_eq!(
            outcome_mid.state,
            HoldState::Satisfied,
            "gate_outcome(gid_mid) must redirect to final gate"
        );

        // And the task was accepted exactly once.
        assert_eq!(ws.accepted_tasks(), vec![task]);
    }
}
