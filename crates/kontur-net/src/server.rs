use std::collections::VecDeque;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, watch, Mutex};

use kontur_core::{HoldState, OperatorId};
use kontur_mcp::{GateHost, HostEvent};

use crate::codec::{read_json, write_json};
use crate::protocol::{ClientMsg, ServerMsg, WireFleetCard, WireGate, WirePhase, WireRole, WireSeat, WireState};

// ---------------------------------------------------------------------------
// Public config
// ---------------------------------------------------------------------------

pub struct SessionConfig {
    pub prompt: String,
    pub plan: Vec<String>,
    pub seats: [(String, OperatorId); 2],
}

// ---------------------------------------------------------------------------
// ScriptedTask / ScriptedAgent — defined here so agent.rs can re-export
// ---------------------------------------------------------------------------

pub struct ScriptedTask {
    pub id: String,
    pub path: String,
    pub contents: String,
}

pub struct ScriptedAgent {
    pub tasks: Vec<ScriptedTask>,
}

impl ScriptedAgent {
    pub fn demo() -> Self {
        ScriptedAgent {
            tasks: vec![
                ScriptedTask {
                    id: "t1".into(),
                    path: "src/guard.rs".into(),
                    contents: "// guard\npub fn guard() {}\n".into(),
                },
                ScriptedTask {
                    id: "t2".into(),
                    path: "src/tokens.rs".into(),
                    contents: "// tokens\npub fn tokens() -> u64 { 0 }\n".into(),
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
enum Phase {
    AwaitOperators,
    DispatchReady,
    PlanReview,
    Executing,
    Closed {
        gates: usize,
        chain_verified: bool,
        reviewers: Vec<String>,
        merged: bool,
    },
}

struct SeatState {
    label: String,
    operator: OperatorId,
    role: WireRole,
    linked: bool,
    ready: bool,
}

struct Net {
    phase: Phase,
    seats: [SeatState; 2],
    fleet: Vec<WireFleetCard>,
    log: VecDeque<String>,
    agent_done: bool,
    finalizing: bool,
    started: Instant,
    agent_plan: Option<Vec<String>>,
}

struct Inner {
    host: Arc<GateHost>,
    cfg: SessionConfig,
    net: Mutex<Net>,
    state_tx: watch::Sender<WireState>,
    plan_tx: watch::Sender<bool>,
}

// ---------------------------------------------------------------------------
// SessionServer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SessionServer {
    inner: Arc<Inner>,
}

impl SessionServer {
    pub fn new(host: Arc<GateHost>, cfg: SessionConfig) -> Self {
        let seats = [
            SeatState {
                label: cfg.seats[0].0.clone(),
                operator: cfg.seats[0].1,
                role: WireRole::Host,
                linked: false,
                ready: false,
            },
            SeatState {
                label: cfg.seats[1].0.clone(),
                operator: cfg.seats[1].1,
                role: WireRole::Operator,
                linked: false,
                ready: false,
            },
        ];

        let initial_state = WireState {
            phase: WirePhase::AwaitOperators,
            seats: vec![
                WireSeat {
                    label: cfg.seats[0].0.clone(),
                    operator: cfg.seats[0].1,
                    role: WireRole::Host,
                    linked: false,
                    ready: false,
                },
                WireSeat {
                    label: cfg.seats[1].0.clone(),
                    operator: cfg.seats[1].1,
                    role: WireRole::Operator,
                    linked: false,
                    ready: false,
                },
            ],
            fleet: vec![],
            log: vec![],
            gate: None,
        };

        let (state_tx, _) = watch::channel(initial_state);
        let (plan_tx, _) = watch::channel(false);

        let net = Net {
            phase: Phase::AwaitOperators,
            seats,
            fleet: vec![],
            log: VecDeque::new(),
            agent_done: false,
            finalizing: false,
            started: Instant::now(),
            agent_plan: None,
        };

        let server = SessionServer {
            inner: Arc::new(Inner {
                host,
                cfg,
                net: Mutex::new(net),
                state_tx,
                plan_tx,
            }),
        };

        // Spawn event pump: translates GateHost activity events into fleet card
        // updates and log lines, then refreshes the console. This is what makes
        // an externally-opened gate visible with no operator keypress.
        {
            let pump_server = server.clone();
            tokio::spawn(async move {
                let agent_id = pump_server.inner.host.agent_id().await;
                let mut rx = pump_server.inner.host.subscribe_events();
                loop {
                    match rx.recv().await {
                        Ok(ev) => pump_server.on_host_event(&agent_id, ev).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        server
    }

    async fn on_host_event(&self, agent_id: &str, ev: HostEvent) {
        match ev {
            HostEvent::Write { path, bytes, .. } => {
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: format!("write {path}"),
                    tokens: 0,
                    needs_signoff: false,
                };
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
                    existing.status = card.status;
                    existing.needs_signoff = false;
                } else {
                    net.fleet.push(card);
                }
                push_log(&mut net, &format!("{agent_id} wrote {path} ({bytes}B)"));
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::Command { command, .. } => {
                let truncated: String = command.chars().take(40).collect();
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: format!("run {truncated}"),
                    tokens: 0,
                    needs_signoff: false,
                };
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
                    existing.status = card.status;
                    existing.needs_signoff = false;
                } else {
                    net.fleet.push(card);
                }
                push_log(&mut net, &format!("{agent_id} ran {truncated}"));
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::GateOpened { gate_id, .. } => {
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: "▶ needs sign-off".to_owned(),
                    tokens: 0,
                    needs_signoff: true,
                };
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
                    existing.status = card.status;
                    existing.needs_signoff = true;
                } else {
                    net.fleet.push(card);
                }
                push_log(&mut net, &format!("gate {} parked at merge gate", gate_id.0));
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::GateResolved { gate_id, state } => {
                // The Cast handler already logs the resolution detail; skip the
                // log line here to avoid duplication. Just update the card.
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == agent_id) {
                    existing.status = "working".to_owned();
                    existing.needs_signoff = false;
                }
                drop(net);
                // Still refresh so the fleet card update reaches clients.
                let _ = state; // acknowledged, cast handler logs it
                let _ = gate_id;
                self.refresh_locked().await;
            }
            HostEvent::PlanProposed { tasks } => {
                let n = tasks.len();
                let mut net = self.inner.net.lock().await;
                net.agent_plan = Some(tasks);
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == agent_id) {
                    existing.status = format!("plan: {n} task(s) awaiting approval");
                    existing.needs_signoff = true;
                } else {
                    net.fleet.push(WireFleetCard {
                        id: agent_id.to_owned(),
                        status: format!("plan: {n} task(s) awaiting approval"),
                        tokens: 0,
                        needs_signoff: true,
                    });
                }
                push_log(&mut net, &format!("agent proposed {n} tasks"));
                drop(net);
                self.refresh_locked().await;
            }
        }
    }

    pub fn state_rx(&self) -> watch::Receiver<WireState> {
        self.inner.state_tx.subscribe()
    }

    pub fn plan_approved_rx(&self) -> watch::Receiver<bool> {
        self.inner.plan_tx.subscribe()
    }

    pub fn host(&self) -> &Arc<GateHost> {
        &self.inner.host
    }

    pub async fn agent_status(&self, card: WireFleetCard) {
        let mut net = self.inner.net.lock().await;
        // Update or insert card
        if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
            *existing = card;
        } else {
            net.fleet.push(card);
        }
        drop(net);
        self.refresh_locked().await;
    }

    pub async fn agent_log(&self, line: String) {
        let mut net = self.inner.net.lock().await;
        push_log(&mut net, &line);
        drop(net);
        self.refresh_locked().await;
    }

    pub async fn agent_done(&self) {
        let mut net = self.inner.net.lock().await;
        net.agent_done = true;
        drop(net);
        self.refresh_locked().await;
    }

    pub async fn attach<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(&self, stream: S) {
        let (read_half, write_half) = tokio::io::split(stream);
        let buf_reader = BufReader::new(read_half);
        let (conn_tx, conn_rx) = mpsc::channel::<ServerMsg>(32);

        let server = self.clone();
        let conn_tx_for_reader = conn_tx.clone();

        // Spawn writer task
        let state_rx = self.inner.state_tx.subscribe();
        tokio::spawn(writer_task(write_half, state_rx, conn_rx));

        // Spawn reader task
        tokio::spawn(reader_task(server, buf_reader, conn_tx_for_reader));
    }
}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

async fn writer_task<W: AsyncWrite + Unpin>(
    mut write_half: W,
    mut state_rx: watch::Receiver<WireState>,
    mut conn_rx: mpsc::Receiver<ServerMsg>,
) {
    // Send the current state immediately on connect
    {
        let state = state_rx.borrow_and_update().clone();
        if write_json(&mut write_half, &ServerMsg::State(Box::new(state)))
            .await
            .is_err()
        {
            return;
        }
    }

    loop {
        tokio::select! {
            result = state_rx.changed() => {
                if result.is_err() {
                    break;
                }
                let state = state_rx.borrow_and_update().clone();
                if write_json(&mut write_half, &ServerMsg::State(Box::new(state))).await.is_err() {
                    break;
                }
            }
            msg = conn_rx.recv() => {
                match msg {
                    Some(m) => {
                        if write_json(&mut write_half, &m).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reader task
// ---------------------------------------------------------------------------

async fn reader_task<R: tokio::io::AsyncBufRead + Unpin + Send + 'static>(
    server: SessionServer,
    mut reader: R,
    conn_tx: mpsc::Sender<ServerMsg>,
) {
    // First message must be Hello
    let hello = match read_json::<_, ClientMsg>(&mut reader).await {
        Ok(Some(msg)) => msg,
        _ => return,
    };

    let (seat_idx, operator) = match &hello {
        ClientMsg::Hello { seat: client_label, operator } => {
            // Seat claim is keyed on OperatorId alone; the configured label
            // for that seat is used everywhere (client-sent label is ignored
            // beyond optional diagnostics).
            let mut found = None;
            let inner = &server.inner;
            for (i, (_label, op)) in inner.cfg.seats.iter().enumerate() {
                if op == operator {
                    found = Some((i, *op));
                    break;
                }
            }
            if found.is_none() {
                // Log the client-sent label for diagnostics only.
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: format!("unknown operator (client seat: {client_label})"),
                    })
                    .await;
                return;
            }
            found.unwrap()
        }
        _ => {
            let _ = conn_tx
                .send(ServerMsg::Rejected {
                    reason: "first message must be Hello".into(),
                })
                .await;
            return;
        }
    };

    // Mark linked
    {
        let mut net = server.inner.net.lock().await;
        net.seats[seat_idx].linked = true;

        // Both linked for first time → advance to DispatchReady
        if net.phase == Phase::AwaitOperators
            && net.seats[0].linked
            && net.seats[1].linked
        {
            net.phase = Phase::DispatchReady;
            push_log(&mut net, "both stations linked");
        }
    }
    server.refresh_locked().await;

    // Send Welcome
    let welcome = ServerMsg::Welcome {
        seat: server.inner.cfg.seats[seat_idx].0.clone(),
    };
    if conn_tx.send(welcome).await.is_err() {
        return;
    }

    // Main read loop
    loop {
        let msg = match read_json::<_, ClientMsg>(&mut reader).await {
            Ok(Some(m)) => m,
            Ok(None) | Err(_) => break,
        };

        handle_client_msg(&server, seat_idx, operator, msg, &conn_tx).await;
    }

    // EOF / disconnected
    {
        let mut net = server.inner.net.lock().await;
        net.seats[seat_idx].linked = false;
        let label = net.seats[seat_idx].label.clone();
        push_log(&mut net, &format!("{label} disconnected · gates park"));
    }
    server.refresh_locked().await;
}

async fn handle_client_msg(
    server: &SessionServer,
    seat_idx: usize,
    operator: OperatorId,
    msg: ClientMsg,
    conn_tx: &mpsc::Sender<ServerMsg>,
) {
    match msg {
        ClientMsg::Hello { .. } => {
            // Ignore re-hellos after initial connection
        }
        ClientMsg::Ready => {
            let mut net = server.inner.net.lock().await;
            net.seats[seat_idx].ready = true;

            let both_ready = net.seats[0].ready && net.seats[1].ready;
            if both_ready {
                match net.phase.clone() {
                    Phase::DispatchReady => {
                        net.phase = Phase::PlanReview;
                        net.seats[0].ready = false;
                        net.seats[1].ready = false;
                        push_log(&mut net, "dispatch confirmed · plan review");
                    }
                    Phase::PlanReview => {
                        // Determine the effective plan: agent-proposed takes priority
                        // over the scripted config plan; if both are empty, refuse.
                        let effective_plan = net.agent_plan.clone().unwrap_or_else(|| server.inner.cfg.plan.clone());
                        if effective_plan.is_empty() {
                            // Consent must be re-signalled against the actual plan (no anchoring).
                            net.seats[0].ready = false;
                            net.seats[1].ready = false;
                            push_log(&mut net, "waiting for agent plan");
                            drop(net);
                            server.refresh_locked().await;
                            return;
                        }
                        net.phase = Phase::Executing;
                        net.seats[0].ready = false;
                        net.seats[1].ready = false;
                        push_log(&mut net, "plan approved · executing");
                        let plan_tx = server.inner.plan_tx.clone();
                        let host = server.inner.host.clone();
                        drop(net);
                        // Approve the real-agent's propose_plan (releases the parked
                        // MCP call). A no-op when no real agent has called propose_plan.
                        host.approve_plan().await;
                        // send_replace, NOT send: watch::Sender::send discards the
                        // value when no receiver is subscribed yet, and the agent
                        // task may not have subscribed under scheduler load — the
                        // approval would be lost and the agent would wait forever.
                        let _ = plan_tx.send_replace(true);
                        server.refresh_locked().await;
                        return;
                    }
                    _ => {}
                }
            }
            drop(net);
            server.refresh_locked().await;
        }
        ClientMsg::Cast { gate_id, verdict } => {
            let label = {
                let net = server.inner.net.lock().await;
                net.seats[seat_idx].label.clone()
            };

            match server.inner.host.submit_verdict(&gate_id, verdict).await {
                Err(e) => {
                    let _ = conn_tx
                        .send(ServerMsg::Rejected { reason: e.to_string() })
                        .await;
                }
                Ok(progress) => {
                    let mut net = server.inner.net.lock().await;
                    push_log(&mut net, &format!("{label} cast · sealed"));
                    match progress.state {
                        HoldState::Satisfied => {
                            push_log(
                                &mut net,
                                &format!("gate {} · both keys in · accepted", gate_id.0),
                            );
                        }
                        HoldState::Blocked => {
                            push_log(
                                &mut net,
                                &format!("gate {} · no-go · remedy routed to agent", gate_id.0),
                            );
                        }
                        _ => {}
                    }
                    drop(net);
                    server.refresh_locked().await;
                }
            }
        }
        ClientMsg::HandEdit { path, contents } => {
            let pending = server.inner.host.pending_gates().await;
            if pending.is_empty() {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "no active gate for hand-edit".into(),
                    })
                    .await;
                return;
            }
            let task_id = pending[0].task_id.clone();
            let label = {
                let net = server.inner.net.lock().await;
                net.seats[seat_idx].label.clone()
            };

            match server
                .inner
                .host
                .hand_edit(task_id, &path, contents.as_bytes(), operator)
                .await
            {
                Err(e) => {
                    let _ = conn_tx
                        .send(ServerMsg::Rejected { reason: e.to_string() })
                        .await;
                }
                Ok(_) => {
                    let mut net = server.inner.net.lock().await;
                    push_log(
                        &mut net,
                        &format!("{label} hand-edit {path} · applied · fresh gate"),
                    );
                    drop(net);
                    server.refresh_locked().await;
                }
            }
        }
        ClientMsg::Bye => {
            // Reader task will handle disconnect naturally when the stream closes
        }
    }
}

// ---------------------------------------------------------------------------
// refresh — rebuild WireState and check finalization
// ---------------------------------------------------------------------------

impl SessionServer {
    async fn refresh_locked(&self) {
        let inner = &self.inner;

        // Check if we need to finalize (before we build the wire state).
        // Atomically claim the finalizing flag so concurrent refreshes
        // cannot both enter finalize().
        let should_finalize = {
            let mut net = inner.net.lock().await;
            if matches!(net.phase, Phase::Executing) && net.agent_done && !net.finalizing {
                net.finalizing = true;
                true
            } else {
                false
            }
        };

        if should_finalize {
            let pending = inner.host.pending_gates().await;
            if pending.is_empty() {
                self.finalize().await;
                return;
            }
            // A gate still pends (e.g. a hand-edit opened after the agent
            // finished): release the claim so the cast that resolves it can
            // finalize on its own refresh. Without this, `finalizing` stays
            // claimed forever and the session can never close.
            inner.net.lock().await.finalizing = false;
        }

        let wire_state = self.build_wire_state().await;
        let _ = inner.state_tx.send(wire_state);
    }

    async fn finalize(&self) {
        let inner = &self.inner;

        // Compose the merge message
        let merge_msg = {
            let net = inner.net.lock().await;
            let first_line = inner
                .cfg
                .prompt
                .lines()
                .next()
                .unwrap_or(&inner.cfg.prompt);
            let op0 = net.seats[0].operator;
            let op1 = net.seats[1].operator;
            let label0 = net.seats[0].label.clone();
            let label1 = net.seats[1].label.clone();
            format!(
                "kontur session: {first_line}\n\nReviewed-by: {label0} {}\nReviewed-by: {label1} {}",
                hex16(&op0),
                hex16(&op1),
            )
        };

        let merged = match inner.host.merge_session(&merge_msg).await {
            Ok(()) => true,
            Err(e) => {
                let mut net = inner.net.lock().await;
                push_log(&mut net, &format!("merge error: {e}"));
                false
            }
        };

        let gates = inner.host.audit_len().await;
        let chain_verified = inner.host.verify_audit().await.is_ok();

        let new_phase = {
            let net = inner.net.lock().await;
            let reviewers = vec![
                net.seats[0].label.clone(),
                net.seats[1].label.clone(),
            ];
            Phase::Closed { gates, chain_verified, reviewers, merged }
        };

        {
            let mut net = inner.net.lock().await;
            net.phase = new_phase;
            push_log(&mut net, "session closed");
        }

        let wire_state = self.build_wire_state().await;
        let _ = inner.state_tx.send(wire_state);
    }

    async fn build_wire_state(&self) -> WireState {
        let inner = &self.inner;
        let net = inner.net.lock().await;

        let wire_phase = match &net.phase {
            Phase::AwaitOperators => WirePhase::AwaitOperators,
            Phase::DispatchReady => WirePhase::DispatchReady {
                prompt: inner.cfg.prompt.clone(),
            },
            Phase::PlanReview => WirePhase::PlanReview {
                tasks: net.agent_plan.clone().unwrap_or_else(|| inner.cfg.plan.clone()),
            },
            Phase::Executing => WirePhase::Executing,
            Phase::Closed { gates, chain_verified, reviewers, merged } => WirePhase::Closed {
                gates: *gates,
                chain_verified: *chain_verified,
                reviewers: reviewers.clone(),
                merged: *merged,
            },
        };

        let wire_seats: Vec<WireSeat> = net
            .seats
            .iter()
            .map(|s| WireSeat {
                label: s.label.clone(),
                operator: s.operator,
                role: s.role,
                linked: s.linked,
                ready: s.ready,
            })
            .collect();

        let fleet = net.fleet.clone();
        let log: Vec<String> = net.log.iter().cloned().collect();

        drop(net);

        // Build gate view from first pending gate
        let gate = {
            let pending = inner.host.pending_gates().await;
            if let Some(gv) = pending.first() {
                let diff_preview = inner
                    .host
                    .gate_diff(&gv.gate_id)
                    .await
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|s| s.chars().take(512).collect::<String>());

                Some(WireGate {
                    gate_id: gv.gate_id.clone(),
                    task: gv.task_id.0.clone(),
                    files: gv.files.clone(),
                    loc: gv.loc,
                    diff_hash: gv.diff_hash,
                    keys: gv.observed.clone(),
                    escalation_required: gv.escalation_required,
                    diff_preview,
                })
            } else {
                None
            }
        };

        WireState {
            phase: wire_phase,
            seats: wire_seats,
            fleet,
            log,
            gate,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn push_log(net: &mut Net, text: &str) {
    let elapsed = net.started.elapsed();
    let secs = elapsed.as_secs();
    let mm = secs / 60;
    let ss = secs % 60;
    let entry = format!("{mm:02}:{ss:02} {text}");
    net.log.push_back(entry);
    while net.log.len() > 8 {
        net.log.pop_front();
    }
}

// Human-readable label only; the verifiable reviewer set lives in the audit chain (reviewed_by).
fn hex16(op: &OperatorId) -> String {
    op.0.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use kontur_core::{
        Ed25519Signer, GateId, Hash, OperatorId, ReviewDepth, Signer, Timestamp,
        Verdict, Remedy,
    };
    use kontur_mcp::{GateHost, InMemoryWorkspace, SessionContext};
    use crate::protocol::{ClientMsg, ServerMsg, WirePhase};
    use crate::codec::{read_json, write_json};
    use tokio::io::BufReader;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    struct TestClock(i64);

    impl kontur_core::Clock for TestClock {
        fn now(&self) -> Timestamp {
            Timestamp(self.0)
        }
    }

    fn cast_go(seed: u8, gate_id: &GateId, dh: Hash) -> kontur_core::CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        kontur_core::CastVerdict::create(
            &signer,
            &TestClock(1000 + seed as i64),
            gate_id,
            dh,
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }

    fn cast_nogo(seed: u8, gate_id: &GateId, dh: Hash, steer: &str) -> kontur_core::CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        kontur_core::CastVerdict::create(
            &signer,
            &TestClock(2000),
            gate_id,
            dh,
            Verdict::NoGo(Remedy::Steer(steer.into())),
            ReviewDepth::FullDiff,
            None,
        )
    }

    fn make_server(
        op1: OperatorId,
        op2: OperatorId,
        tasks: Vec<String>,
    ) -> (SessionServer, Arc<InMemoryWorkspace>) {
        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new(
            "fix the thing",
            op1,
            "agent-01",
            "claude",
            "1.0",
            vec![op1, op2],
        );
        let host = Arc::new(GateHost::new(ctx, ws.clone()));
        let cfg = SessionConfig {
            prompt: "fix the thing".into(),
            plan: tasks,
            seats: [("A".into(), op1), ("B".into(), op2)],
        };
        let server = SessionServer::new(host, cfg);
        (server, ws)
    }

    /// Wait until the watch receiver's current-or-next state satisfies the predicate.
    /// Checks the current state first before waiting for changes.
    async fn wait_for_state<F>(
        state_rx: &mut watch::Receiver<WireState>,
        predicate: F,
    ) -> WireState
    where
        F: Fn(&WireState) -> bool,
    {
        loop {
            {
                let s = state_rx.borrow_and_update().clone();
                if predicate(&s) {
                    return s;
                }
            }
            // Not yet — wait for the next change
            if state_rx.changed().await.is_err() {
                panic!("state_tx dropped");
            }
        }
    }

    /// Drain the read side of a client duplex so the server's writer doesn't block.
    async fn drain_client<R: tokio::io::AsyncBufRead + Unpin + Send + 'static>(
        reader: R,
    ) {
        tokio::spawn(async move {
            let mut r = reader;
            while let Ok(Some(_)) = read_json::<_, ServerMsg>(&mut r).await {}
        });
    }

    // -----------------------------------------------------------------------
    // Test 1: two_operators_full_arc
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn two_operators_full_arc() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, ws) = make_server(op1, op2, vec!["guard.rs".into()]);
        let mut state_rx = server.state_rx();

        // Create 1-task scripted agent
        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/guard.rs".into(),
                contents: "// guard\n".into(),
            }],
        };

        // Spawn agent
        let server_for_agent = server.clone();
        tokio::spawn(crate::agent::run_agent(agent, server_for_agent));

        // Create duplex pairs
        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);

        server.attach(server_a).await;
        server.attach(server_b).await;

        // Split client halves
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);

        // Drain read sides
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        // Send Hello from both
        write_json(&mut ca_write, &ClientMsg::Hello { seat: "A".into(), operator: op1 })
            .await
            .unwrap();
        write_json(&mut cb_write, &ClientMsg::Hello { seat: "B".into(), operator: op2 })
            .await
            .unwrap();

        // Wait for DispatchReady
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        }))
        .await
        .expect("timed out waiting for DispatchReady");

        // Both ready → PlanReview
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        }))
        .await
        .expect("timed out waiting for PlanReview");

        // Both ready → Executing
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Executing)
        }))
        .await
        .expect("timed out waiting for Executing");

        // Wait for a gate
        let state_with_gate = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.gate.is_some()
        }))
        .await
        .expect("timed out waiting for gate");

        let wire_gate = state_with_gate.gate.unwrap();
        let gate_id = wire_gate.gate_id.clone();
        let diff_hash = wire_gate.diff_hash;

        // A casts go
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(1, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        // Wait for at least one key sealed
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.gate.as_ref().map(|g| !g.keys.is_empty()).unwrap_or(false)
        }))
        .await
        .expect("timed out waiting for A key sealed");

        // B casts go
        write_json(
            &mut cb_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(2, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        // Wait for Closed
        let final_state = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Closed { .. })
        }))
        .await
        .expect("timed out waiting for Closed");

        match &final_state.phase {
            WirePhase::Closed { chain_verified, merged, .. } => {
                assert!(chain_verified, "chain should be verified");
                assert!(merged, "session should have merged successfully");
            }
            _ => panic!("expected Closed phase"),
        }

        let msg = ws.merged_message().expect("should have a merge message");
        assert!(msg.contains("Reviewed-by: A"), "merge message should contain A");
        assert!(msg.contains("Reviewed-by: B"), "merge message should contain B");
    }

    // -----------------------------------------------------------------------
    // Test 2: nogo_routes_remedy_and_agent_reworks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn nogo_routes_remedy_and_agent_reworks() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["guard.rs".into()]);
        let mut state_rx = server.state_rx();

        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/guard.rs".into(),
                contents: "// guard\n".into(),
            }],
        };

        let server_for_agent = server.clone();
        tokio::spawn(crate::agent::run_agent(agent, server_for_agent));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);

        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);

        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(&mut ca_write, &ClientMsg::Hello { seat: "A".into(), operator: op1 })
            .await
            .unwrap();
        write_json(&mut cb_write, &ClientMsg::Hello { seat: "B".into(), operator: op2 })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        }))
        .await
        .expect("timed out waiting for DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        }))
        .await
        .expect("timed out waiting for PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Executing)
        }))
        .await
        .expect("timed out waiting for Executing");

        // Wait for first gate
        let state_with_gate = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.gate.is_some()
        }))
        .await
        .expect("timed out waiting for first gate");

        let wire_gate = state_with_gate.gate.unwrap();
        let gate_id = wire_gate.gate_id.clone();
        let diff_hash = wire_gate.diff_hash;

        // A goes, B no-go
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(1, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        write_json(
            &mut cb_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_nogo(2, &gate_id, diff_hash, "add caching"),
            },
        )
        .await
        .unwrap();

        // Wait for a new gate (agent reworks and opens a second gate)
        let state_with_new_gate = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.gate.as_ref().map(|g| g.gate_id != gate_id).unwrap_or(false)
        }))
        .await
        .expect("timed out waiting for rework gate");

        let rework_gate = state_with_new_gate.gate.unwrap();
        let rework_gate_id = rework_gate.gate_id.clone();
        let rework_diff_hash = rework_gate.diff_hash;

        // Both go on rework gate
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: rework_gate_id.clone(),
                verdict: cast_go(1, &rework_gate_id, rework_diff_hash),
            },
        )
        .await
        .unwrap();

        write_json(
            &mut cb_write,
            &ClientMsg::Cast {
                gate_id: rework_gate_id.clone(),
                verdict: cast_go(2, &rework_gate_id, rework_diff_hash),
            },
        )
        .await
        .unwrap();

        // Wait for Closed with gates == 2
        let final_state = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Closed { .. })
        }))
        .await
        .expect("timed out waiting for Closed");

        match &final_state.phase {
            WirePhase::Closed { gates, .. } => {
                assert_eq!(*gates, 2, "should have 2 audit records");
            }
            _ => panic!("expected Closed phase"),
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: disconnect_parks_and_reconnect_resumes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn disconnect_parks_and_reconnect_resumes() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["guard.rs".into()]);
        let mut state_rx = server.state_rx();

        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/guard.rs".into(),
                contents: "// guard\n".into(),
            }],
        };

        let server_for_agent = server.clone();
        tokio::spawn(crate::agent::run_agent(agent, server_for_agent));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);

        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);

        drain_client(BufReader::new(ca_read)).await;

        // Keep B's read side to drain later
        let cb_buf_reader = BufReader::new(cb_read);

        write_json(&mut ca_write, &ClientMsg::Hello { seat: "A".into(), operator: op1 })
            .await
            .unwrap();
        write_json(&mut cb_write, &ClientMsg::Hello { seat: "B".into(), operator: op2 })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        }))
        .await
        .expect("timed out waiting for DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        }))
        .await
        .expect("timed out waiting for PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Executing)
        }))
        .await
        .expect("timed out waiting for Executing");

        // Wait for gate
        let state_with_gate = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.gate.is_some()
        }))
        .await
        .expect("timed out waiting for gate");

        let wire_gate = state_with_gate.gate.unwrap();
        let gate_id = wire_gate.gate_id.clone();
        let diff_hash = wire_gate.diff_hash;

        // Drop B's entire client handle — simulates disconnect.
        // We must drop both halves; the drain task holds cb_read, so we
        // need to stop the drain task first by dropping the reader.
        drop(cb_buf_reader);
        drop(cb_write);

        // Wait for B to show linked=false
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.seats.iter().any(|seat| seat.label == "B" && !seat.linked)
        }))
        .await
        .expect("timed out waiting for B disconnect");

        // A tries to cast alone — gate should stay open (not closed)
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(1, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        // A's cast triggers a refresh/broadcast. Await that next state
        // deterministically and assert: still Executing AND the gate has
        // exactly one key entry (A's, Sealed) — proving no resolution happened.
        let after_a_cast = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate.as_ref().map(|g| !g.keys.is_empty()).unwrap_or(false)
            }),
        )
        .await
        .expect("timed out waiting for A's key to be recorded");

        assert!(
            !matches!(after_a_cast.phase, WirePhase::Closed { .. }),
            "gate should not close with only one key"
        );
        assert_eq!(
            after_a_cast.gate.as_ref().map(|g| g.keys.len()).unwrap_or(0),
            1,
            "exactly one key (A's, Sealed) should be recorded"
        );

        // Reconnect B with a new duplex
        let (client_b2, server_b2) = tokio::io::duplex(65536);
        server.attach(server_b2).await;

        let (cb2_read, mut cb2_write) = tokio::io::split(client_b2);
        drain_client(BufReader::new(cb2_read)).await;

        write_json(&mut cb2_write, &ClientMsg::Hello { seat: "B".into(), operator: op2 })
            .await
            .unwrap();

        // Wait for B linked again
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            s.seats.iter().any(|seat| seat.label == "B" && seat.linked)
        }))
        .await
        .expect("timed out waiting for B reconnect");

        // B casts → Closed
        write_json(
            &mut cb2_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(2, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Closed { .. })
        }))
        .await
        .expect("timed out waiting for Closed");
    }

    // -----------------------------------------------------------------------
    // Test 4: finalize_is_idempotent_under_concurrent_refresh (Fix 1 regression)
    //
    // After agent_done with no pending gates, calling agent_done (and thus
    // refresh_locked) twice concurrently must result in the session closing
    // exactly once — i.e. the log contains exactly one "session closed" entry.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn finalize_is_idempotent_under_concurrent_refresh() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        // Non-empty plan so both-ready transitions through PlanReview.
        let (server, ws) = make_server(op1, op2, vec!["dummy".into()]);
        let mut state_rx = server.state_rx();

        // Drive both seats through to Executing manually (no agent tasks).
        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);

        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(&mut ca_write, &ClientMsg::Hello { seat: "A".into(), operator: op1 })
            .await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Hello { seat: "B".into(), operator: op2 })
            .await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })).await.expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        })).await.expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Executing)
        })).await.expect("Executing");

        // Trigger two concurrent agent_done calls. The finalizing flag must
        // ensure only one of them proceeds through finalize().
        let s1 = server.clone();
        let s2 = server.clone();
        let (h1, h2) = tokio::join!(
            tokio::spawn(async move { s1.agent_done().await }),
            tokio::spawn(async move { s2.agent_done().await }),
        );
        h1.unwrap();
        h2.unwrap();

        // Wait for Closed.
        let closed_state = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(s.phase, WirePhase::Closed { .. })
        })).await.expect("timed out waiting for Closed");

        // The merge message must be present (finalize ran at least once).
        assert!(ws.merged_message().is_some(), "merge message should be set");

        // The log must contain exactly one "session closed" entry.
        let closed_count = closed_state.log.iter().filter(|l| l.contains("session closed")).count();
        assert_eq!(
            closed_count, 1,
            "expected exactly one 'session closed' log entry, got {closed_count}: {:?}",
            closed_state.log
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: external_agent_activity_streams_without_operator_action
    //
    // Verifies that calling host.record_write and host.begin_task_gate
    // directly (exactly as the MCP KonturServer handler does) causes the
    // console to refresh — specifically: the WireState watch advances to a
    // state where gate.is_some() AND the log contains a "wrote" line, with
    // NO operator client messages sent after the initial Hello.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn external_agent_activity_streams_without_operator_action() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        // Attach ONE duplex operator client for Hello only.
        let (client_a, server_a) = tokio::io::duplex(65536);
        server.attach(server_a).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        drain_client(BufReader::new(ca_read)).await;

        // Send Hello — phase stays AwaitOperators / DispatchReady (only one seat).
        write_json(&mut ca_write, &ClientMsg::Hello { seat: "A".into(), operator: op1 })
            .await
            .unwrap();

        // Allow the Hello to be processed.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Directly call the host methods that the MCP KonturServer handler calls.
        let task = kontur_core::TaskId("t1".into());
        server.host().record_write(&task, "main.rs", b"fn main() {}\n").await.unwrap();
        server.host().begin_task_gate(task, 0).await.unwrap();

        // Wait — without any further operator messages — for a WireState where
        // gate.is_some() AND the log contains a "wrote" line. This proves the
        // event pump refreshed the console without operator input.
        let matched = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate.is_some()
                    && s.log.iter().any(|l| l.contains("wrote"))
            }),
        )
        .await
        .expect("timed out waiting for gate + wrote log line without operator action");

        assert!(matched.gate.is_some(), "gate must be present");
        assert!(
            matched.log.iter().any(|l| l.contains("wrote")),
            "log must contain a 'wrote' line; log = {:?}",
            matched.log
        );
    }
}
