use std::collections::VecDeque;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, watch, Mutex};

use kontur_core::{HoldState, OperatorId};
use kontur_mcp::{GateHost, HostEvent};

use crate::codec::{read_json, write_json};
use crate::protocol::{
    ClientMsg, ServerMsg, WireCmd, WireComment, WireFileDiff, WireFleetCard, WireGate,
    WirePendingJoin, WirePhase, WireQuestion, WireRole, WireSeat, WireState,
};

/// How long the server waits for any client traffic before treating the peer
/// as gone. Set to 3× the client heartbeat so a couple of dropped pings don't
/// falsely park a live session.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

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

/// Outcome of a host's decision on a pending BYO operator's key.
#[derive(Clone, PartialEq, Eq, Debug)]
enum JoinDecision {
    Pending,
    Approved(OperatorId),
    Rejected(OperatorId),
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Phase {
    AwaitOperators,
    DispatchReady,
    PlanReview,
    Clarify,
    Executing,
    Closed {
        gates: usize,
        chain_verified: bool,
        reviewers: Vec<String>,
        merged: bool,
        abandoned: bool,
    },
}

struct SeatState {
    label: String,
    operator: OperatorId,
    role: WireRole,
    linked: bool,
    ready: bool,
    afk: bool,
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
    /// Live session prompt. Initialised from cfg.prompt; updated in-console by
    /// SetPrompt during DispatchReady. After dispatch it is locked — the agent
    /// is running against the text that was actually consented to.
    prompt: String,
    /// Most recent command + exit code per task id, for the gate card.
    last_cmd: std::collections::HashMap<String, WireCmd>,
    /// Soft presence claim on the active gate: (gate_id, seat_idx). Cleared
    /// when the gate resolves (id no longer matches) or the claimer drops.
    claim: Option<(String, usize)>,
    /// Gate discussion notes, keyed by gate id: (seat_idx, text) in order.
    discuss: std::collections::HashMap<String, Vec<(usize, String)>>,
    /// Active clarification exchange, while the agent is awaiting answers.
    clarify: Option<crate::clarify::Clarify>,
    /// BYO seat B: the operator key bound at approval (None until approved).
    /// Only meaningful when seat B is configured as BYO (zero sentinel op).
    seat_b_bound: Option<OperatorId>,
    /// A BYO operator's key currently awaiting the host's approval.
    pending_join: Option<OperatorId>,
}

struct Inner {
    host: Arc<GateHost>,
    cfg: SessionConfig,
    net: Mutex<Net>,
    state_tx: watch::Sender<WireState>,
    plan_tx: watch::Sender<bool>,
    /// Resolves a pending BYO join once the host approves or rejects it.
    join_tx: watch::Sender<JoinDecision>,
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
                afk: false,
            },
            SeatState {
                label: cfg.seats[1].0.clone(),
                operator: cfg.seats[1].1,
                role: WireRole::Operator,
                linked: false,
                ready: false,
                afk: false,
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
                    afk: false,
                },
                WireSeat {
                    label: cfg.seats[1].0.clone(),
                    operator: cfg.seats[1].1,
                    role: WireRole::Operator,
                    linked: false,
                    ready: false,
                    afk: false,
                },
            ],
            fleet: vec![],
            log: vec![],
            gate: None,
            prompt: String::new(),
            pending_join: None,
        };

        let (state_tx, _) = watch::channel(initial_state);
        let (plan_tx, _) = watch::channel(false);
        let (join_tx, _) = watch::channel(JoinDecision::Pending);

        let net = Net {
            phase: Phase::AwaitOperators,
            seats,
            fleet: vec![],
            log: VecDeque::new(),
            agent_done: false,
            finalizing: false,
            started: Instant::now(),
            agent_plan: None,
            prompt: cfg.prompt.clone(),
            last_cmd: std::collections::HashMap::new(),
            claim: None,
            discuss: std::collections::HashMap::new(),
            clarify: None,
            seat_b_bound: None,
            pending_join: None,
        };

        let server = SessionServer {
            inner: Arc::new(Inner {
                host,
                cfg,
                net: Mutex::new(net),
                state_tx,
                plan_tx,
                join_tx,
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
            HostEvent::Write {
                agent, path, bytes, ..
            } => {
                let agent_id = agent.as_str();
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: format!("write {path}"),
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
            HostEvent::Command {
                agent,
                task,
                command,
                exit_code,
            } => {
                let agent_id = agent.as_str();
                let truncated: String = command.chars().take(40).collect();
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: format!("run {truncated}"),
                    needs_signoff: false,
                };
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
                    existing.status = card.status;
                    existing.needs_signoff = false;
                } else {
                    net.fleet.push(card);
                }
                net.last_cmd.insert(
                    task.0.clone(),
                    WireCmd {
                        command: command.clone(),
                        exit_code,
                    },
                );
                // Outcome, not just invocation: a failed command is
                // decision-relevant and must not read like a passing one.
                if exit_code == 0 {
                    push_log(&mut net, &format!("{agent_id} ran {truncated} · exit 0"));
                } else {
                    push_log(
                        &mut net,
                        &format!("{agent_id} ran {truncated} · FAILED exit {exit_code}"),
                    );
                }
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::GateOpened { gate_id, .. } => {
                let card = WireFleetCard {
                    id: agent_id.to_owned(),
                    status: "▶ needs sign-off".to_owned(),
                    needs_signoff: true,
                };
                let mut net = self.inner.net.lock().await;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == card.id) {
                    existing.status = card.status;
                    existing.needs_signoff = true;
                } else {
                    net.fleet.push(card);
                }
                push_log(
                    &mut net,
                    &format!("gate {} parked at merge gate", gate_id.0),
                );
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
            HostEvent::GateSuperseded {
                old_gate_id,
                new_gate_id,
            } => {
                // The stale pending hold has been removed; the fresh gate now
                // carries the combined diff. Log and refresh so the wire
                // projects the fresh gate immediately — realtime property.
                let mut net = self.inner.net.lock().await;
                push_log(
                    &mut net,
                    &format!(
                        "gate {} superseded by hand-edit → {}",
                        old_gate_id.0, new_gate_id.0
                    ),
                );
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::PlanProposed { agent, tasks } => {
                let agent_id = agent.as_str();
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
                        needs_signoff: true,
                    });
                }
                push_log(&mut net, &format!("agent proposed {n} tasks"));
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::PlanSteered { steer } => {
                let mut net = self.inner.net.lock().await;
                push_log(
                    &mut net,
                    &format!(
                        "plan steer routed to agent: {}",
                        steer.chars().take(40).collect::<String>()
                    ),
                );
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::QuestionsAsked { agent, questions } => {
                let agent_id = agent.as_str();
                let n = questions.len();
                let reducer = crate::clarify::Clarify::new(
                    questions
                        .into_iter()
                        .map(|q| crate::clarify::Question {
                            prompt: q.prompt,
                            options: q.options,
                        })
                        .collect(),
                );
                let mut net = self.inner.net.lock().await;
                net.clarify = Some(reducer);
                net.phase = Phase::Clarify;
                if let Some(existing) = net.fleet.iter_mut().find(|c| c.id == agent_id) {
                    existing.status = format!("awaiting {n} clarification answer(s)");
                    existing.needs_signoff = true;
                }
                push_log(
                    &mut net,
                    &format!("agent asked {n} clarification question(s)"),
                );
                drop(net);
                self.refresh_locked().await;
            }
            HostEvent::SessionAbandoned => {
                // The abandon handler in handle_client_msg already logs and
                // transitions the phase; nothing extra to do here.
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

    /// Return the current session prompt. During DispatchReady this may differ
    /// from the CLI-time prompt (operators can edit it in-console). After
    /// dispatch it is locked to the text both seats consented to.
    pub async fn session_prompt(&self) -> String {
        self.inner.net.lock().await.prompt.clone()
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

/// Resolve a BYO seat-B join: reconnect if already bound to this key, reject if
/// bound to a different key, otherwise hold pending until the host approves the
/// fingerprint. Returns `Some(1)` once seated, or `None` if rejected/closed.
/// How long a BYO join may wait for host approval before the slot is freed —
/// generous enough to read a fingerprint out-of-band, bounded so a pending
/// operator that quietly disconnects cannot block seat B forever.
const BYO_APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

async fn resolve_byo_join(
    server: &SessionServer,
    operator: OperatorId,
    conn_tx: &mpsc::Sender<ServerMsg>,
) -> Option<usize> {
    let mut rx = server.inner.join_tx.subscribe();
    {
        let mut net = server.inner.net.lock().await;
        match net.seat_b_bound {
            Some(bound) if bound == operator => return Some(1), // approved reconnect
            Some(_) => {
                drop(net);
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "seat B is already bound to a different key".into(),
                    })
                    .await;
                return None;
            }
            None => {}
        }
        // Serialize admission: at most one *distinct* key awaits approval at a
        // time. "First approval wins the seat" is then unambiguous — the host is
        // never shown one fingerprint while a different key is parked behind it,
        // so an approval cannot bind the wrong key.
        if let Some(other) = net.pending_join {
            if other != operator {
                drop(net);
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "another operator is already awaiting approval — try again shortly"
                            .into(),
                    })
                    .await;
                return None;
            }
        }
        net.pending_join = Some(operator);
        push_log(
            &mut net,
            &format!(
                "Operator B requests approval · fingerprint {}",
                operator.fingerprint()
            ),
        );
    }
    server.refresh_locked().await;
    let _ = conn_tx
        .send(ServerMsg::AwaitingApproval {
            fingerprint: operator.fingerprint(),
        })
        .await;

    // Bounded wait for the host's decision about THIS key.
    let outcome = tokio::time::timeout(BYO_APPROVAL_TIMEOUT, async {
        loop {
            match rx.borrow_and_update().clone() {
                JoinDecision::Approved(op) if op == operator => return Some(true),
                JoinDecision::Rejected(op) if op == operator => return Some(false),
                _ => {}
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    })
    .await;

    let mut net = server.inner.net.lock().await;
    if net.pending_join == Some(operator) {
        net.pending_join = None;
    }
    let mut newly_bound = false;
    let result = match outcome {
        // First approval wins the seat, re-checked under the lock.
        Ok(Some(true)) => match net.seat_b_bound {
            Some(bound) if bound == operator => Some(1),
            Some(_) => None,
            None => {
                net.seat_b_bound = Some(operator);
                // Bind the approved key into seat B's identity so display, the
                // Reviewed-by trailers, and the audit roster reflect the real
                // operator rather than the zero placeholder configured for BYO.
                net.seats[1].operator = operator;
                newly_bound = true;
                Some(1)
            }
        },
        _ => None,
    };
    drop(net);
    if newly_bound {
        // Register with the gate host (its own lock) so hand-edit eligibility
        // and session_operators include the approved key.
        server.inner.host.register_operator(operator).await;
    }
    server.refresh_locked().await;
    if result.is_none() {
        let reason = match outcome {
            Ok(Some(false)) => "join rejected by host",
            Ok(Some(true)) => "seat B is already bound to a different key",
            _ => "approval timed out",
        };
        let _ = conn_tx
            .send(ServerMsg::Rejected {
                reason: reason.into(),
            })
            .await;
    }
    result
}

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

    let (operator, protocol, client_label) = match &hello {
        ClientMsg::Hello {
            seat,
            operator,
            protocol,
        } => (*operator, *protocol, seat.clone()),
        _ => {
            let _ = conn_tx
                .send(ServerMsg::Rejected {
                    reason: "first message must be Hello".into(),
                })
                .await;
            return;
        }
    };

    // Version gate first: a build mismatch must fail here with a clear
    // message, not later with an opaque deserialization error.
    if protocol != crate::protocol::PROTOCOL_VERSION {
        let _ = conn_tx
            .send(ServerMsg::Rejected {
                reason: format!(
                    "protocol mismatch — update kontur (server v{}, client v{})",
                    crate::protocol::PROTOCOL_VERSION,
                    protocol
                ),
            })
            .await;
        return;
    }

    // The zero sentinel is only a configuration marker for a BYO seat, never a
    // real identity — reject it as an incoming key so it cannot bypass approval
    // by matching the configured (sentinel) seat directly.
    if operator == OperatorId([0u8; 32]) {
        let _ = conn_tx
            .send(ServerMsg::Rejected {
                reason: "invalid operator identity".into(),
            })
            .await;
        return;
    }

    // Seat claim is keyed on OperatorId. A key matching a configured seat is
    // seated directly. An unmatched key is accepted only for a BYO seat B
    // (configured with the zero sentinel), and only after the host approves it.
    let direct = server
        .inner
        .cfg
        .seats
        .iter()
        .position(|(_, op)| *op == operator);
    let seat_idx = if let Some(i) = direct {
        i
    } else if server.inner.cfg.seats[1].1 == OperatorId([0u8; 32]) {
        // BYO seat B (zero sentinel). Approval gates attachment; verdict
        // acceptance is separately constrained to the registered operator set
        // (GateHost::submit_verdict) and each cast is bound to the connection's
        // authenticated identity, so an approved fingerprint is load-bearing at
        // the merge boundary.
        match resolve_byo_join(&server, operator, &conn_tx).await {
            Some(i) => i,
            None => return,
        }
    } else {
        let _ = conn_tx
            .send(ServerMsg::Rejected {
                reason: format!("unknown operator (client seat: {client_label})"),
            })
            .await;
        return;
    };

    // Mark linked
    {
        let mut net = server.inner.net.lock().await;
        net.seats[seat_idx].linked = true;

        // Both linked for first time → advance to DispatchReady
        if net.phase == Phase::AwaitOperators && net.seats[0].linked && net.seats[1].linked {
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

    // Main read loop. A read that produces nothing within READ_TIMEOUT means
    // the peer is gone (a half-open TCP connection reads as silence, not EOF);
    // the client's Ping every HEARTBEAT_INTERVAL keeps a live link inside the
    // window. Either way, falling out of the loop parks the seat's gates.
    loop {
        let read = tokio::time::timeout(READ_TIMEOUT, read_json::<_, ClientMsg>(&mut reader)).await;
        let msg = match read {
            Ok(Ok(Some(m))) => m,
            // EOF, decode error, or no traffic within the timeout → gone.
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        };
        handle_client_msg(&server, seat_idx, operator, msg, &conn_tx).await;
    }

    // EOF / timeout / disconnected
    {
        let mut net = server.inner.net.lock().await;
        net.seats[seat_idx].linked = false;
        // A dropped seat is not "AFK" — that's a distinct, present state; clear
        // it so a reconnecting operator starts attending.
        net.seats[seat_idx].afk = false;
        // Drop this seat's claim so a stale "reviewing" marker doesn't linger.
        if matches!(&net.claim, Some((_, idx)) if *idx == seat_idx) {
            net.claim = None;
        }
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
                        // A blank instruction cannot be dispatched. Same
                        // anchoring rule as the empty-plan refusal: consent
                        // must be re-signalled once a prompt actually exists.
                        if net.prompt.trim().is_empty() {
                            net.seats[0].ready = false;
                            net.seats[1].ready = false;
                            push_log(&mut net, "prompt is empty — compose with [p]");
                            drop(net);
                            server.refresh_locked().await;
                            return;
                        }
                        net.phase = Phase::PlanReview;
                        net.seats[0].ready = false;
                        net.seats[1].ready = false;
                        push_log(&mut net, "dispatch confirmed · plan review");
                        // Authoritative re-push: the prompt may have arrived
                        // only as live drafts (never committed via SetPrompt),
                        // so hand the gate host exactly the text both seats
                        // consented to — same sync-point pattern as the plan.
                        let prompt = net.prompt.clone();
                        drop(net);
                        server.inner.host.set_prompt(prompt).await;
                        server.refresh_locked().await;
                        return;
                    }
                    Phase::PlanReview => {
                        // Determine the effective plan: agent-proposed takes priority
                        // over the scripted config plan; if both are empty, refuse.
                        let effective_plan = net
                            .agent_plan
                            .clone()
                            .unwrap_or_else(|| server.inner.cfg.plan.clone());
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
                        // Authoritative re-push — the agent must receive exactly the
                        // list the wire gated on. EditPlan's own set_plan is advisory/
                        // display-path only; this both-ready arm is the single sync point
                        // that guarantees the stored plan matches what both seats approved.
                        host.set_plan(effective_plan).await;
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
            // Bind the verdict to the connection's authenticated identity: a
            // seat may only cast as itself. Without this, gate acceptance would
            // rest on any two distinct valid signatures rather than on the two
            // authenticated (and, for BYO seat B, host-approved) seat keys —
            // load-bearing for the four-eyes guarantee.
            if verdict.operator != operator {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "verdict identity does not match your seat".into(),
                    })
                    .await;
                return;
            }
            let label = {
                let net = server.inner.net.lock().await;
                net.seats[seat_idx].label.clone()
            };

            match server.inner.host.submit_verdict(&gate_id, verdict).await {
                Err(e) => {
                    // Surface SessionAbandoned as a Rejected reason so the
                    // casting operator knows post-abandon casts are closed.
                    let _ = conn_tx
                        .send(ServerMsg::Rejected {
                            reason: e.to_string(),
                        })
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
                        .send(ServerMsg::Rejected {
                            reason: e.to_string(),
                        })
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
        ClientMsg::Abandon => {
            let label = {
                let net = server.inner.net.lock().await;
                net.seats[seat_idx].label.clone()
            };

            // Guard: if already Closed, ignore (race with finalize or double-abandon).
            {
                let mut net = server.inner.net.lock().await;
                if matches!(net.phase, Phase::Closed { .. }) {
                    return;
                }
                // Claim finalizing regardless — abandon wins races with finalize.
                net.finalizing = true;
            }

            // Discard all pending tasks via GateHost.
            if let Err(e) = server.inner.host.abandon_session().await {
                let mut net = server.inner.net.lock().await;
                push_log(&mut net, &format!("abandon error: {e}"));
            }

            let gates = server.inner.host.audit_len().await;
            let chain_verified = server.inner.host.verify_audit().await.is_ok();

            // Resolved gates are evidence even when nothing merges — persist
            // the chain on abandon as well.
            match server.inner.host.persist_audit().await {
                Some(Ok(path)) => {
                    let mut net = server.inner.net.lock().await;
                    push_log(
                        &mut net,
                        &format!("audit chain written · {}", path.display()),
                    );
                }
                Some(Err(e)) => {
                    let mut net = server.inner.net.lock().await;
                    push_log(&mut net, &format!("AUDIT WRITE FAILED: {e}"));
                }
                None => {}
            }

            let new_phase = {
                let net = server.inner.net.lock().await;
                let reviewers = vec![net.seats[0].label.clone(), net.seats[1].label.clone()];
                Phase::Closed {
                    gates,
                    chain_verified,
                    reviewers,
                    merged: false,
                    abandoned: true,
                }
            };

            {
                let mut net = server.inner.net.lock().await;
                net.phase = new_phase;
                push_log(
                    &mut net,
                    &format!("{label} abandoned the session — nothing merged"),
                );
            }

            server.refresh_locked().await;
        }
        ClientMsg::FetchFile { path } => {
            // Requires an active pending gate — the file belongs to the task
            // under review. Same guard as HandEdit.
            let pending = server.inner.host.pending_gates().await;
            if pending.is_empty() {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "no active gate — nothing to fetch".into(),
                    })
                    .await;
                return;
            }
            let task_id = pending[0].task_id.clone();
            match server.inner.host.read_file(&task_id, &path).await {
                Err(e) => {
                    let _ = conn_tx
                        .send(ServerMsg::Rejected {
                            reason: e.to_string(),
                        })
                        .await;
                }
                Ok(maybe_bytes) => {
                    match maybe_bytes {
                        None => {
                            // File does not exist in the worktree (new file).
                            let _ = conn_tx
                                .send(ServerMsg::FileContent {
                                    path,
                                    contents: None,
                                })
                                .await;
                        }
                        Some(bytes) => {
                            match std::str::from_utf8(&bytes) {
                                Ok(text) => {
                                    let _ = conn_tx
                                        .send(ServerMsg::FileContent {
                                            path,
                                            contents: Some(text.to_owned()),
                                        })
                                        .await;
                                }
                                Err(_) => {
                                    // Binary file: cannot round-trip through a text editor.
                                    // Send FileContent with contents: None AND a Rejected
                                    // notice so the operator knows to hand-edit on the host.
                                    let _ = conn_tx
                                        .send(ServerMsg::FileContent {
                                            path: path.clone(),
                                            contents: None,
                                        })
                                        .await;
                                    let _ = conn_tx
                                        .send(ServerMsg::Rejected {
                                            reason: "binary file — hand-edit on the host machine"
                                                .into(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }
        ClientMsg::Claim { gate_id } => {
            let mut net = server.inner.net.lock().await;
            // Toggle: same seat releases; otherwise this seat takes it.
            net.claim = match &net.claim {
                Some((gid, idx)) if gid == &gate_id.0 && *idx == seat_idx => None,
                _ => Some((gate_id.0.clone(), seat_idx)),
            };
            let label = net.seats[seat_idx].label.clone();
            match &net.claim {
                Some(_) => push_log(&mut net, &format!("{label} is reviewing {}", gate_id.0)),
                None => push_log(&mut net, &format!("{label} released {}", gate_id.0)),
            }
            drop(net);
            server.refresh_locked().await;
        }
        ClientMsg::Answer { question, choice } => {
            // Apply the answer to the clarify reducer; when every question is
            // resolved, hand the accepted answers back to the agent and move on.
            // Phase/reducer are cleared inside the SAME lock as the resolution
            // check so a concurrent Answer sees `phase != Clarify` and bails —
            // no double resolve, no duplicate log line.
            let resolved: Option<Vec<Vec<String>>> = {
                let mut net = server.inner.net.lock().await;
                if net.phase != Phase::Clarify {
                    return;
                }
                let Some(clarify) = net.clarify.as_mut() else {
                    return;
                };
                let choice = match choice {
                    crate::protocol::WireChoice::Option(i) => crate::clarify::Choice::Option(i),
                    crate::protocol::WireChoice::Custom(s) => crate::clarify::Choice::Custom(s),
                };
                clarify.answer(seat_idx, question, choice);
                let resolved = clarify.resolved();
                if resolved.is_some() {
                    net.clarify = None;
                    net.phase = Phase::PlanReview;
                    push_log(&mut net, "clarification resolved · awaiting agent plan");
                }
                resolved
            };
            if let Some(answers) = resolved {
                // Unblock the parked ask_clarification MCP call.
                server.inner.host.resolve_clarification(answers).await;
            }
            server.refresh_locked().await;
        }
        ClientMsg::Discuss { gate_id, text } => {
            let text = text.trim().to_owned();
            if !text.is_empty() {
                let mut net = server.inner.net.lock().await;
                let label = net.seats[seat_idx].label.clone();
                net.discuss
                    .entry(gate_id.0.clone())
                    .or_default()
                    .push((seat_idx, text.clone()));
                push_log(&mut net, &format!("{label} noted on {}: {text}", gate_id.0));
                drop(net);
                server.refresh_locked().await;
            }
        }
        ClientMsg::ResolveJoin { operator, approve } => {
            // Only the host seat may approve or reject a BYO join.
            if seat_idx != 0 {
                return;
            }
            server.inner.join_tx.send_replace(if approve {
                JoinDecision::Approved(operator)
            } else {
                JoinDecision::Rejected(operator)
            });
            let mut net = server.inner.net.lock().await;
            push_log(
                &mut net,
                &format!(
                    "host {} Operator B ({})",
                    if approve { "approved" } else { "rejected" },
                    operator.fingerprint()
                ),
            );
            drop(net);
            server.refresh_locked().await;
        }
        ClientMsg::SetAfk { afk } => {
            let mut net = server.inner.net.lock().await;
            if net.seats[seat_idx].afk != afk {
                net.seats[seat_idx].afk = afk;
                let label = net.seats[seat_idx].label.clone();
                push_log(
                    &mut net,
                    &format!("{label} is {}", if afk { "AFK" } else { "back" }),
                );
                drop(net);
                server.refresh_locked().await;
            }
        }
        ClientMsg::Ping => {
            // Liveness only: arrival already reset the read timeout; reply so a
            // client can detect a dead host in turn. Never touches gate state.
            let _ = conn_tx.send(ServerMsg::Pong).await;
        }
        ClientMsg::Bye => {
            // Reader task will handle disconnect naturally when the stream closes
        }
        ClientMsg::EditPlan { tasks } => {
            let mut net = server.inner.net.lock().await;
            // Plan edits are only valid during PlanReview. Once the agent is
            // executing the plan is locked — the agent is working against the
            // task list both seats consented to.
            if net.phase != Phase::PlanReview {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "plan is locked".into(),
                    })
                    .await;
                return;
            }
            // Guard: the list must be non-empty and every task non-blank.
            if tasks.is_empty() || tasks.iter().any(|t| t.trim().is_empty()) {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "plan tasks cannot be empty".into(),
                    })
                    .await;
                return;
            }
            let label = net.seats[seat_idx].label.clone();
            let n = tasks.len();
            net.agent_plan = Some(tasks.clone());
            // Anchoring rule: any edit resets both ready flags so both seats
            // must re-consent against the plan they actually see.
            net.seats[0].ready = false;
            net.seats[1].ready = false;
            push_log(&mut net, &format!("{label} edited the plan ({n} tasks)"));
            drop(net);
            // NOTE: set_plan is intentionally NOT called here. The authoritative
            // sync point is the Phase::PlanReview both-ready arm, which calls
            // set_plan(effective_plan) immediately before approve_plan(). That
            // guarantees the agent receives exactly the list both seats signed —
            // no race between this advisory update and the approval path.
            // The wire update (net.agent_plan) and ready-flag reset above are
            // the only effects EditPlan needs.
            server.refresh_locked().await;
        }
        ClientMsg::SteerPlan { steer } => {
            let mut net = server.inner.net.lock().await;
            // Only valid during PlanReview.
            if net.phase != Phase::PlanReview {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "plan is locked".into(),
                    })
                    .await;
                return;
            }
            // No bare veto — steer must have content.
            if steer.trim().is_empty() {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "steer cannot be empty".into(),
                    })
                    .await;
                return;
            }
            let label = net.seats[seat_idx].label.clone();
            // Reset both ready flags (re-consent required for the new plan).
            net.seats[0].ready = false;
            net.seats[1].ready = false;
            // Withdraw the current plan list — returns to "waiting for agent plan…"
            // until the agent re-proposes.
            net.agent_plan = None;
            push_log(&mut net, &format!("{label} steered a replan"));
            drop(net);
            // Route the steer to the agent via gatehost (lock already dropped).
            server.inner.host.steer_plan(steer).await;
            server.refresh_locked().await;
        }
        ClientMsg::SetPrompt { prompt } => {
            let mut net = server.inner.net.lock().await;
            // Prompt edits are only valid before dispatch. Once the agent is
            // running, the prompt is locked — consent must be re-signalled
            // against the text actually shown (same anchoring rule as plan gate).
            if net.phase != Phase::DispatchReady {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "prompt is locked after dispatch".into(),
                    })
                    .await;
                return;
            }
            if prompt.trim().is_empty() {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "prompt cannot be empty".into(),
                    })
                    .await;
                return;
            }
            let label = net.seats[seat_idx].label.clone();
            net.prompt = prompt.clone();
            // Anchoring rule: any edit resets both ready flags so both seats
            // must re-consent against the text they actually see. This prevents
            // a seat from marking ready against one prompt and having the other
            // seat silently dispatch a different one.
            net.seats[0].ready = false;
            net.seats[1].ready = false;
            push_log(&mut net, &format!("{label} edited the prompt"));
            drop(net);
            server.inner.host.set_prompt(prompt).await;
            server.refresh_locked().await;
        }
        ClientMsg::PromptDraft { prompt } => {
            let mut net = server.inner.net.lock().await;
            // Same lock as SetPrompt: no draft traffic after dispatch.
            if net.phase != Phase::DispatchReady {
                let _ = conn_tx
                    .send(ServerMsg::Rejected {
                        reason: "prompt is locked after dispatch".into(),
                    })
                    .await;
                return;
            }
            // Live sync: the other seat sees each keystroke. Anchoring rule
            // still applies — any change resets both ready flags — but a
            // draft is not a logged event (the SetPrompt commit is). The
            // gate host receives the authoritative text at dispatch.
            net.prompt = prompt;
            net.seats[0].ready = false;
            net.seats[1].ready = false;
            drop(net);
            server.refresh_locked().await;
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
            let first_line = net
                .prompt
                .lines()
                .next()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| net.prompt.clone());
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

        // Anchor the merge commit to the audit chain: persist the signed
        // records, then carry the chain head in a trailer so the commit and
        // the evidence reference each other.
        let audit_head = inner.host.audit_head().await;
        let head_hex: String = audit_head.0.iter().map(|b| format!("{b:02x}")).collect();
        let merge_msg = format!("{merge_msg}\nAudit-chain: sha256:{head_hex}");
        match inner.host.persist_audit().await {
            Some(Ok(path)) => {
                let mut net = inner.net.lock().await;
                push_log(
                    &mut net,
                    &format!("audit chain written · {}", path.display()),
                );
            }
            Some(Err(e)) => {
                let mut net = inner.net.lock().await;
                push_log(&mut net, &format!("AUDIT WRITE FAILED: {e}"));
            }
            None => {}
        }

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
            let reviewers = vec![net.seats[0].label.clone(), net.seats[1].label.clone()];
            Phase::Closed {
                gates,
                chain_verified,
                reviewers,
                merged,
                abandoned: false,
            }
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
                prompt: net.prompt.clone(),
            },
            Phase::PlanReview => WirePhase::PlanReview {
                tasks: net
                    .agent_plan
                    .clone()
                    .unwrap_or_else(|| inner.cfg.plan.clone()),
            },
            Phase::Clarify => WirePhase::Clarify {
                questions: net
                    .clarify
                    .as_ref()
                    .map(|c| {
                        (0..c.len())
                            .map(|q| WireQuestion {
                                prompt: c.prompt(q),
                                options: c.options(q),
                                allows_custom: c.allows_custom(q),
                                picks: [c.pick(0, q), c.pick(1, q)],
                                resolved: c.resolved_answer(q),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            Phase::Executing => WirePhase::Executing,
            Phase::Closed {
                gates,
                chain_verified,
                reviewers,
                merged,
                abandoned,
            } => WirePhase::Closed {
                gates: *gates,
                chain_verified: *chain_verified,
                reviewers: reviewers.clone(),
                merged: *merged,
                abandoned: *abandoned,
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
                afk: s.afk,
            })
            .collect();

        let fleet = net.fleet.clone();
        let log: Vec<String> = net.log.iter().cloned().collect();
        let last_cmds = net.last_cmd.clone();
        let prompt_snapshot = net.prompt.clone();
        let claim_snapshot = net.claim.clone();
        let discuss_snapshot = net.discuss.clone();
        let pending_join_snapshot = net.pending_join;
        let seat_labels = [net.seats[0].label.clone(), net.seats[1].label.clone()];

        drop(net);

        // Build gate view from first pending gate
        let gate = {
            let pending = inner.host.pending_gates().await;
            if let Some(gv) = pending.first() {
                // Wire sanity cap: each file's diff section is independently
                // capped at 64 KiB so one giant generated file (package-lock)
                // cannot exhaust the connection buffer or starve the other
                // files' diffs. Operators who need the full file can fetch it
                // with FetchFile. `truncated` is set per file, and the gate's
                // diff_truncated when any section was capped, so the TUI warns
                // before accepting a go on a partial diff.
                const FILE_DIFF_CAP: usize = 64 * 1024;
                let file_diffs: Vec<WireFileDiff> = inner
                    .host
                    .gate_diff(&gv.gate_id)
                    .await
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|s| {
                        let fallback = match gv.files.as_slice() {
                            [only] => only.clone(),
                            _ => "(all files)".to_owned(),
                        };
                        crate::difftext::split_file_diffs_or_whole(&s, &fallback)
                            .into_iter()
                            .map(|fd| cap_file_diff(fd, FILE_DIFF_CAP))
                            .collect()
                    })
                    .unwrap_or_default();
                let diff_truncated = file_diffs.iter().any(|fd| fd.truncated);

                Some(WireGate {
                    gate_id: gv.gate_id.clone(),
                    task: gv.task_id.0.clone(),
                    files: gv.files.clone(),
                    loc: gv.loc,
                    diff_hash: gv.diff_hash,
                    keys: gv.observed.clone(),
                    escalation_required: gv.escalation_required,
                    file_diffs,
                    diff_truncated,
                    last_cmd: last_cmds.get(&gv.task_id.0).cloned(),
                    agent: gv.agent.clone(),
                    claimed_by: claim_snapshot.as_ref().and_then(|(gid, idx)| {
                        (gid == &gv.gate_id.0).then(|| seat_labels[*idx].clone())
                    }),
                    discuss: discuss_snapshot
                        .get(&gv.gate_id.0)
                        .map(|notes| {
                            notes
                                .iter()
                                .map(|(idx, text)| WireComment {
                                    who: seat_labels[*idx].clone(),
                                    text: text.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
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
            prompt: prompt_snapshot,
            pending_join: pending_join_snapshot.map(|operator| WirePendingJoin {
                operator,
                fingerprint: operator.fingerprint(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Cap one file's diff section for the wire. Each file is capped
/// independently so a single huge generated file cannot starve the other
/// files' diffs or exhaust the connection buffer.
fn cap_file_diff(fd: crate::difftext::FileDiff, cap: usize) -> WireFileDiff {
    if fd.diff.chars().count() > cap {
        let capped: String = fd.diff.chars().take(cap).collect();
        WireFileDiff {
            path: fd.path,
            diff: format!("{capped}\n… (diff truncated)"),
            truncated: true,
        }
    } else {
        WireFileDiff {
            path: fd.path,
            diff: fd.diff,
            truncated: false,
        }
    }
}

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
    use crate::codec::{read_json, write_json};
    use crate::protocol::{ClientMsg, ServerMsg, WirePhase};

    /// Per-file cap: a huge file truncates only itself; small files ride
    /// alongside untouched.
    #[test]
    fn cap_applies_per_file_not_across_files() {
        let small = crate::difftext::FileDiff {
            path: "src/a.rs".into(),
            diff: "diff --git a/src/a.rs b/src/a.rs\n+small\n".into(),
        };
        let huge = crate::difftext::FileDiff {
            path: "package-lock.json".into(),
            diff: format!("diff --git a/p b/p\n{}", "x".repeat(200)),
        };
        let capped_small = cap_file_diff(small, 100);
        let capped_huge = cap_file_diff(huge, 100);
        assert!(!capped_small.truncated);
        assert!(capped_small.diff.contains("+small"));
        assert!(capped_huge.truncated);
        assert!(capped_huge.diff.ends_with("… (diff truncated)"));
        assert!(capped_huge.diff.chars().count() < 200);
    }
    use kontur_core::{
        Ed25519Signer, GateId, Hash, OperatorId, Remedy, ReviewDepth, Signer, Timestamp, Verdict,
    };
    use kontur_mcp::{GateHost, InMemoryWorkspace, SessionContext};
    use std::time::Duration;
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
        make_server_with_prompt(op1, op2, tasks, "fix the thing")
    }

    fn make_server_with_prompt(
        op1: OperatorId,
        op2: OperatorId,
        tasks: Vec<String>,
        prompt: &str,
    ) -> (SessionServer, Arc<InMemoryWorkspace>) {
        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new(prompt, op1, "agent-01", "claude", "1.0", vec![op1, op2]);
        let host = Arc::new(GateHost::new(ctx, ws.clone()));
        let cfg = SessionConfig {
            prompt: prompt.into(),
            plan: tasks,
            seats: [("A".into(), op1), ("B".into(), op2)],
        };
        let server = SessionServer::new(host, cfg);
        (server, ws)
    }

    /// A server in BYO mode: seat B's configured operator is the zero
    /// sentinel, so an unknown key must be host-approved to seat.
    fn make_byo_server(op_host: OperatorId) -> (SessionServer, Arc<InMemoryWorkspace>) {
        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new("byo", op_host, "agent-01", "claude", "1.0", vec![op_host]);
        let host = Arc::new(GateHost::new(ctx, ws.clone()));
        let cfg = SessionConfig {
            prompt: "byo".into(),
            plan: vec!["t1".into()],
            seats: [
                ("Operator A [Host]".into(), op_host),
                ("Operator B".into(), OperatorId([0u8; 32])),
            ],
        };
        (SessionServer::new(host, cfg), ws)
    }

    /// BYO join: an unknown key is held pending (fingerprint surfaced to the
    /// host), and the host's approval seats it; a reconnect skips re-approval.
    #[tokio::test]
    async fn byo_join_pends_then_host_approval_seats() {
        let op_host = Ed25519Signer::from_seed([1; 32]).operator_id();
        let byo = Ed25519Signer::from_seed([9; 32]).operator_id();
        let (server, _ws) = make_byo_server(op_host);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        let mut cb_reader = BufReader::new(cb_read);

        // Host connects normally.
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op_host,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // BYO operator connects with its OWN (unknown) key.
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: byo,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // The operator receives AwaitingApproval with its fingerprint.
        let mut got_awaiting = false;
        for _ in 0..8 {
            match read_json::<_, ServerMsg>(&mut cb_reader).await.unwrap() {
                Some(ServerMsg::AwaitingApproval { fingerprint }) => {
                    assert_eq!(fingerprint, byo.fingerprint());
                    got_awaiting = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(got_awaiting, "operator must be told it awaits approval");

        // The host sees the pending join on the wire, and B is NOT yet linked.
        let pend = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.pending_join.is_some()),
        )
        .await
        .expect("pending join surfaced");
        assert_eq!(pend.pending_join.as_ref().unwrap().operator, byo);
        assert!(!pend.seats.get(1).map(|x| x.linked).unwrap_or(false));

        // Host approves.
        write_json(
            &mut ca_write,
            &ClientMsg::ResolveJoin {
                operator: byo,
                approve: true,
            },
        )
        .await
        .unwrap();

        // B links, pending clears, and both linked → DispatchReady.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.pending_join.is_none()
                    && s.seats.get(1).map(|x| x.linked).unwrap_or(false)
                    && matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("approved BYO operator seats and dispatch opens");

        // Seat B's identity on the wire is now the approved key (not the zero
        // placeholder), and the gate host's roster includes it — so trailers,
        // audit, and hand-edit eligibility reference the real operator.
        let bound = state_rx.borrow().clone();
        assert_eq!(
            bound.seats[1].operator, byo,
            "seat B bound to the approved key"
        );
        assert!(
            server.host().session_operators().await.contains(&byo),
            "approved key registered in the session roster"
        );
    }

    /// Admission is serialized: while one BYO key awaits approval, a second
    /// distinct key is rejected (so an approval can never bind the wrong key).
    #[tokio::test]
    async fn byo_second_pending_key_is_rejected() {
        let op_host = Ed25519Signer::from_seed([1; 32]).operator_id();
        let byo_x = Ed25519Signer::from_seed([9; 32]).operator_id();
        let byo_y = Ed25519Signer::from_seed([8; 32]).operator_id();
        let (server, _ws) = make_byo_server(op_host);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_x, server_x) = tokio::io::duplex(65536);
        let (client_y, server_y) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_x).await;
        server.attach(server_y).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cx_read, mut cx_write) = tokio::io::split(client_x);
        let (cy_read, mut cy_write) = tokio::io::split(client_y);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cx_read)).await;
        let mut cy_reader = BufReader::new(cy_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op_host,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cx_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: byo_x,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // X becomes the pending join.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.pending_join
                    .as_ref()
                    .map(|p| p.operator == byo_x)
                    .unwrap_or(false)
            }),
        )
        .await
        .expect("X pending");

        // Y arrives while X is pending → Y is rejected, X stays pending.
        write_json(
            &mut cy_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: byo_y,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        let mut y_rejected = false;
        for _ in 0..12 {
            match read_json::<_, ServerMsg>(&mut cy_reader).await.unwrap() {
                Some(ServerMsg::Rejected { reason }) => {
                    assert!(
                        reason.contains("already awaiting approval"),
                        "got: {reason}"
                    );
                    y_rejected = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(y_rejected, "second distinct pending key must be rejected");
        // X is still the (only) pending join.
        let s = state_rx.borrow().clone();
        assert_eq!(s.pending_join.as_ref().unwrap().operator, byo_x);
    }

    /// A Hello presenting the zero sentinel as its identity is rejected — it
    /// must not seat directly by matching a BYO seat's configured sentinel.
    #[tokio::test]
    async fn sentinel_identity_is_rejected_at_hello() {
        let op_host = Ed25519Signer::from_seed([1; 32]).operator_id();
        let (server, _ws) = make_byo_server(op_host);

        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_b).await;
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        let mut cb_reader = BufReader::new(cb_read);

        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: OperatorId([0u8; 32]),
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        let mut rejected = false;
        for _ in 0..8 {
            match read_json::<_, ServerMsg>(&mut cb_reader).await.unwrap() {
                Some(ServerMsg::Rejected { reason }) => {
                    assert!(reason.contains("invalid operator"), "got: {reason}");
                    rejected = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(
            rejected,
            "the sentinel identity must be rejected, never seated"
        );
    }

    /// Capstone: an approved BYO operator can actually sign a verdict the gate
    /// accepts — approval registers the key end-to-end.
    #[tokio::test]
    async fn approved_byo_key_can_cast_and_satisfy() {
        let op_host = Ed25519Signer::from_seed([1; 32]).operator_id();
        let byo = Ed25519Signer::from_seed([9; 32]).operator_id();
        let (server, ws) = make_byo_server(op_host);
        let mut state_rx = server.state_rx();

        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/guard.rs".into(),
                contents: "// guard\n".into(),
            }],
        };
        tokio::spawn(crate::agent::run_agent(agent, server.clone()));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op_host,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: byo,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Approve the BYO key.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.pending_join.is_some()),
        )
        .await
        .expect("pending");
        write_json(
            &mut ca_write,
            &ClientMsg::ResolveJoin {
                operator: byo,
                approve: true,
            },
        )
        .await
        .unwrap();

        // Dispatch + plan through to a gate.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        let gated = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");
        let wg = gated.gate.unwrap();
        let gid = wg.gate_id;
        let dh = wg.diff_hash;

        // The approved BYO operator (seed 9) casts go, signed with its own key;
        // the host (seed 1) casts go; the gate accepts and the task merges.
        write_json(
            &mut cb_write,
            &ClientMsg::Cast {
                gate_id: gid.clone(),
                verdict: cast_go(9, &gid, dh),
            },
        )
        .await
        .unwrap();
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gid.clone(),
                verdict: cast_go(1, &gid, dh),
            },
        )
        .await
        .unwrap();

        // Session closes merged — both keys satisfied the gate.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("closed after both keys");
        assert!(
            !ws.accepted_tasks().is_empty(),
            "the approved BYO key's verdict satisfied the gate"
        );
    }

    /// A rejected BYO key is closed, not seated.
    #[tokio::test]
    async fn byo_join_rejected_is_closed() {
        let op_host = Ed25519Signer::from_seed([1; 32]).operator_id();
        let byo = Ed25519Signer::from_seed([9; 32]).operator_id();
        let (server, _ws) = make_byo_server(op_host);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        let mut cb_reader = BufReader::new(cb_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op_host,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: byo,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.pending_join.is_some()),
        )
        .await
        .expect("pending");

        write_json(
            &mut ca_write,
            &ClientMsg::ResolveJoin {
                operator: byo,
                approve: false,
            },
        )
        .await
        .unwrap();

        // Operator receives a Rejected and B never links.
        let mut got_reject = false;
        for _ in 0..12 {
            match read_json::<_, ServerMsg>(&mut cb_reader).await.unwrap() {
                Some(ServerMsg::Rejected { reason }) => {
                    assert!(reason.contains("rejected"), "got: {reason}");
                    got_reject = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(got_reject, "rejected operator must be told");
        let s = state_rx.borrow().clone();
        assert!(s.pending_join.is_none());
        assert!(!s.seats.get(1).map(|x| x.linked).unwrap_or(false));
    }

    /// Wait until the watch receiver's current-or-next state satisfies the predicate.
    /// Checks the current state first before waiting for changes.
    async fn wait_for_state<F>(state_rx: &mut watch::Receiver<WireState>, predicate: F) -> WireState
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
    async fn drain_client<R: tokio::io::AsyncBufRead + Unpin + Send + 'static>(reader: R) {
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
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Wait for DispatchReady
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("timed out waiting for DispatchReady");

        // Both ready → PlanReview
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("timed out waiting for PlanReview");

        // Both ready → Executing
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("timed out waiting for Executing");

        // Wait for a gate
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
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
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate.as_ref().map(|g| !g.keys.is_empty()).unwrap_or(false)
            }),
        )
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
        let final_state = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("timed out waiting for Closed");

        match &final_state.phase {
            WirePhase::Closed {
                chain_verified,
                merged,
                ..
            } => {
                assert!(chain_verified, "chain should be verified");
                assert!(merged, "session should have merged successfully");
            }
            _ => panic!("expected Closed phase"),
        }

        let msg = ws.merged_message().expect("should have a merge message");
        assert!(
            msg.contains("Audit-chain: sha256:"),
            "merge message must anchor the audit chain head; got:\n{msg}"
        );
        assert!(
            msg.contains("Reviewed-by: A"),
            "merge message should contain A"
        );
        assert!(
            msg.contains("Reviewed-by: B"),
            "merge message should contain B"
        );
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

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("timed out waiting for DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("timed out waiting for PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("timed out waiting for Executing");

        // Wait for first gate
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
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
        let state_with_new_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate
                    .as_ref()
                    .map(|g| g.gate_id != gate_id)
                    .unwrap_or(false)
            }),
        )
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
        let final_state = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("timed out waiting for Closed");

        match &final_state.phase {
            WirePhase::Closed { gates, .. } => {
                assert_eq!(*gates, 2, "should have 2 audit records");
            }
            _ => panic!("expected Closed phase"),
        }
    }

    /// A seat cannot cast a verdict signed by a key other than the one it
    /// authenticated with — the verdict identity must match the seat.
    #[tokio::test]
    async fn cast_with_foreign_identity_is_rejected() {
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
        tokio::spawn(crate::agent::run_agent(agent, server.clone()));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(cb_read)).await;
        let mut ca_reader = BufReader::new(ca_read);

        for (w, seat, op) in [(&mut ca_write, "A", op1), (&mut cb_write, "B", op2)] {
            write_json(
                w,
                &ClientMsg::Hello {
                    seat: seat.into(),
                    operator: op,
                    protocol: crate::protocol::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        let gated = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");
        let wg = gated.gate.unwrap();
        let gid = wg.gate_id;
        let dh = wg.diff_hash;

        // Seat A tries to cast a verdict signed by op2 (not its authenticated key).
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gid.clone(),
                verdict: cast_go(2, &gid, dh),
            },
        )
        .await
        .unwrap();

        // The rejection with an identity reason arrives on A's stream.
        let mut got = false;
        for _ in 0..40 {
            match tokio::time::timeout(
                Duration::from_millis(200),
                read_json::<_, ServerMsg>(&mut ca_reader),
            )
            .await
            {
                Ok(Ok(Some(ServerMsg::Rejected { reason }))) if reason.contains("identity") => {
                    got = true;
                    break;
                }
                Ok(Ok(Some(_))) => continue,
                _ => break,
            }
        }
        assert!(got, "a foreign-identity cast must be rejected");
    }

    /// A Ping is answered with a Pong and is never gated — pure liveness.
    #[tokio::test]
    async fn ping_is_answered_with_pong() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);

        let (client_a, server_a) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(&mut ca_write, &ClientMsg::Ping).await.unwrap();

        let mut got_pong = false;
        for _ in 0..6 {
            match read_json::<_, ServerMsg>(&mut ca_reader).await.unwrap() {
                Some(ServerMsg::Pong) => {
                    got_pong = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(got_pong, "server must answer Ping with Pong");
    }

    /// A silent peer (no traffic, no heartbeat) is parked once the read timeout
    /// elapses — the half-open case. Uses paused virtual time so the 45s window
    /// passes instantly.
    #[tokio::test(start_paused = true)]
    async fn silent_peer_times_out_and_parks() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        // Both link (raw duplexes — no client heartbeat task), then go silent.
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.seats.iter().all(|seat| seat.linked)),
        )
        .await
        .expect("both linked");

        // Advance past the read timeout with no traffic: both seats park.
        tokio::time::advance(READ_TIMEOUT + Duration::from_secs(1)).await;

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.seats.iter().all(|seat| !seat.linked)),
        )
        .await
        .expect("silent peers must park after the read timeout");
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

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("timed out waiting for DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("timed out waiting for PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("timed out waiting for Executing");

        // Wait for gate
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
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
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.iter().any(|seat| seat.label == "B" && !seat.linked)
            }),
        )
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
            after_a_cast
                .gate
                .as_ref()
                .map(|g| g.keys.len())
                .unwrap_or(0),
            1,
            "exactly one key (A's, Sealed) should be recorded"
        );

        // Reconnect B with a new duplex
        let (client_b2, server_b2) = tokio::io::duplex(65536);
        server.attach(server_b2).await;

        let (cb2_read, mut cb2_write) = tokio::io::split(client_b2);
        drain_client(BufReader::new(cb2_read)).await;

        write_json(
            &mut cb2_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Wait for B linked again
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.iter().any(|seat| seat.label == "B" && seat.linked)
            }),
        )
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

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
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

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

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
        let closed_state = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("timed out waiting for Closed");

        // The merge message must be present (finalize ran at least once).
        assert!(ws.merged_message().is_some(), "merge message should be set");

        // The log must contain exactly one "session closed" entry.
        let closed_count = closed_state
            .log
            .iter()
            .filter(|l| l.contains("session closed"))
            .count();
        assert_eq!(
            closed_count, 1,
            "expected exactly one 'session closed' log entry, got {closed_count}: {:?}",
            closed_state.log
        );
    }

    // -----------------------------------------------------------------------
    // Test: abandon_mid_gate_discards_and_closes
    //
    // One seat sends Abandon while a gate is open. The session must:
    //   - transition to Closed { abandoned: true, merged: false }
    //   - discard the pending task
    //   - leave the audit chain intact (any pre-existing records still verify)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn abandon_mid_gate_discards_and_closes() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, ws) = make_server(op1, op2, vec!["guard.rs".into()]);
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

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Wait for a gate to appear.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");

        // A sends Abandon while the gate is open.
        write_json(&mut ca_write, &ClientMsg::Abandon)
            .await
            .unwrap();

        // Session must close with abandoned=true, merged=false.
        let final_state = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("timed out waiting for Closed after Abandon");

        match &final_state.phase {
            WirePhase::Closed {
                merged, abandoned, ..
            } => {
                assert!(*abandoned, "phase must be abandoned=true");
                assert!(!merged, "nothing should be merged on abandon");
            }
            _ => panic!("expected Closed phase"),
        }

        // The pending task was discarded.
        assert!(
            !ws.discarded_tasks().is_empty(),
            "pending task must have been discarded"
        );

        // The audit chain (which had no resolved gates yet) still verifies.
        assert!(
            server.inner.host.verify_audit().await.is_ok(),
            "audit chain must remain intact after abandon"
        );

        // The session log mentions the abandon.
        assert!(
            final_state.log.iter().any(|l| l.contains("abandoned")),
            "log must mention abandon; log = {:?}",
            final_state.log
        );
    }

    // -----------------------------------------------------------------------
    // Test: abandon_after_close_is_ignored
    //
    // Sending Abandon when the session is already Closed must be a no-op.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn abandon_after_close_is_ignored() {
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

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Wait for a gate.
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");

        let wire_gate = state_with_gate.gate.unwrap();
        let gate_id = wire_gate.gate_id.clone();
        let diff_hash = wire_gate.diff_hash;

        // Both cast go → normal close.
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
                verdict: cast_go(2, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        // Wait for normal Closed.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("normal Closed");

        // Now send Abandon — must be ignored (no double-transition).
        write_json(&mut ca_write, &ClientMsg::Abandon)
            .await
            .unwrap();

        // Give the server a moment to process (if it were going to do anything).
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The final state must still be a normal close: abandoned=false, merged=true.
        let current = state_rx.borrow().clone();
        match &current.phase {
            WirePhase::Closed {
                abandoned, merged, ..
            } => {
                assert!(
                    !abandoned,
                    "phase must not flip to abandoned after normal close"
                );
                assert!(merged, "session must remain merged=true");
            }
            _ => panic!("expected still Closed"),
        }

        // Log must contain exactly one close-type entry mentioning "session closed"
        // (no "abandoned" line appended after the fact).
        let abandon_count = current
            .log
            .iter()
            .filter(|l| l.contains("abandoned"))
            .count();
        assert_eq!(
            abandon_count, 0,
            "no abandon log line expected; log = {:?}",
            current.log
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
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Allow the Hello to be processed.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Directly call the host methods that the MCP KonturServer handler calls.
        let task = kontur_core::TaskId("t1".into());
        server
            .host()
            .record_write("agent-01", &task, "main.rs", b"fn main() {}\n")
            .await
            .unwrap();
        server
            .host()
            .begin_task_gate("agent-01", task, 0)
            .await
            .unwrap();

        // Wait — without any further operator messages — for a WireState where
        // gate.is_some() AND the log contains a "wrote" line. This proves the
        // event pump refreshed the console without operator input.
        let matched = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate.is_some() && s.log.iter().any(|l| l.contains("wrote"))
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

    // -----------------------------------------------------------------------
    // T5 race-fix: post-abandon cast receives Rejected with "abandoned" reason
    //
    // After Abandon closes the session, a Cast message arriving on the same
    // connection must produce a Rejected response whose reason contains
    // "abandoned". This proves the SessionAbandoned guard is wired through
    // handle_client_msg → submit_verdict → ServerMsg::Rejected.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn post_abandon_cast_receives_rejected() {
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

        // Split client A into read/write. We keep the read side live so we
        // can inspect Rejected messages sent back to A.
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);

        // B's read side is only drained (we don't need to inspect B's messages).
        drain_client(BufReader::new(cb_read)).await;

        // Wrap A's read side so we can call read_json on it.
        let mut ca_reader = BufReader::new(ca_read);

        // Send Hello from both
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Drain incoming server messages on A until DispatchReady.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Wait for a gate.
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");

        let wire_gate = state_with_gate.gate.unwrap();
        let gate_id = wire_gate.gate_id.clone();
        let diff_hash = wire_gate.diff_hash;

        // A sends Abandon.
        write_json(&mut ca_write, &ClientMsg::Abandon)
            .await
            .unwrap();

        // Wait for the session to close.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Closed { .. })
            }),
        )
        .await
        .expect("Closed after Abandon");

        // Drain all outstanding state messages from A's read side so we can
        // spot the Rejected response to the upcoming Cast.
        // (The writer task sends state snapshots; we want to get past them.)
        let (drain_tx, mut drain_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        let drain_tx_clone = drain_tx.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut ca_reader).await {
                let _ = drain_tx_clone.send(msg).await;
            }
        });

        // Now A casts on the abandoned gate — should produce a Rejected.
        write_json(
            &mut ca_write,
            &ClientMsg::Cast {
                gate_id: gate_id.clone(),
                verdict: cast_go(1, &gate_id, diff_hash),
            },
        )
        .await
        .unwrap();

        // Wait for a Rejected message whose reason contains "abandoned".
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got_rejected = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, drain_rx.recv()).await {
                Ok(Some(ServerMsg::Rejected { reason })) if reason.contains("abandoned") => {
                    got_rejected = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            got_rejected,
            "post-abandon Cast must produce Rejected with 'abandoned' in reason"
        );
    }

    // -----------------------------------------------------------------------
    // SetPrompt tests
    // -----------------------------------------------------------------------

    /// SetPrompt during DispatchReady:
    /// - updates the wire prompt in the next WireState
    /// - resets both ready flags (seat A readied; B edits; A's ready must be false)
    #[tokio::test]
    async fn set_prompt_updates_wire_and_resets_ready() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // A marks ready.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.first().map(|s| s.ready).unwrap_or(false)
            }),
        )
        .await
        .expect("A ready");

        // B edits the prompt — should reset both ready flags.
        write_json(
            &mut cb_write,
            &ClientMsg::SetPrompt {
                prompt: "new prompt text".into(),
            },
        )
        .await
        .unwrap();

        let after_edit = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(&s.phase, WirePhase::DispatchReady { prompt } if prompt == "new prompt text")
        })).await.expect("prompt updated in wire state");

        // Both ready flags must have been reset.
        assert!(
            !after_edit.seats[0].ready,
            "A ready must be reset after prompt edit"
        );
        assert!(
            !after_edit.seats[1].ready,
            "B ready must be reset after prompt edit"
        );

        // The wire prompt must carry the new text.
        match &after_edit.phase {
            WirePhase::DispatchReady { prompt } => assert_eq!(prompt, "new prompt text"),
            _ => panic!("expected DispatchReady"),
        }
    }

    /// SetPrompt after dispatch (PlanReview phase) must return Rejected with
    /// the reason "prompt is locked after dispatch".
    #[tokio::test]
    async fn set_prompt_after_dispatch_rejected() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        // Keep A's read side to capture Rejected.
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(cb_read)).await;

        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both ready → PlanReview.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Drain A's read side into a channel so we can inspect Rejected.
        let (drain_tx, mut drain_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        let drain_tx_clone = drain_tx.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut ca_reader).await {
                let _ = drain_tx_clone.send(msg).await;
            }
        });

        // SetPrompt in PlanReview phase → must be Rejected.
        write_json(
            &mut ca_write,
            &ClientMsg::SetPrompt {
                prompt: "too late".into(),
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got_locked = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, drain_rx.recv()).await {
                Ok(Some(ServerMsg::Rejected { reason })) if reason.contains("locked") => {
                    got_locked = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            got_locked,
            "SetPrompt after dispatch must be Rejected with 'locked' in reason"
        );
    }

    /// SetPrompt with an empty/whitespace prompt must return Rejected with
    /// "prompt cannot be empty".
    #[tokio::test]
    async fn set_prompt_empty_rejected() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(cb_read)).await;

        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        let (drain_tx, mut drain_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        let drain_tx_clone = drain_tx.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut ca_reader).await {
                let _ = drain_tx_clone.send(msg).await;
            }
        });

        // Empty prompt → Rejected.
        write_json(
            &mut ca_write,
            &ClientMsg::SetPrompt {
                prompt: "   ".into(),
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got_empty_rejected = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, drain_rx.recv()).await {
                Ok(Some(ServerMsg::Rejected { reason })) if reason.contains("empty") => {
                    got_empty_rejected = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            got_empty_rejected,
            "empty SetPrompt must produce Rejected with 'empty' in reason"
        );
    }

    /// End-to-end: the agent asks a clarification question; both operators
    /// answer (agreeing); the exchange resolves, the parked ask_clarification
    /// future returns the answers, and the phase returns to PlanReview.
    #[tokio::test]
    async fn clarification_resolves_when_both_answer() {
        use kontur_mcp::ClarifyQuestion;

        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        for (w, seat, op) in [(&mut ca_write, "A", op1), (&mut cb_write, "B", op2)] {
            write_json(
                w,
                &ClientMsg::Hello {
                    seat: seat.into(),
                    operator: op,
                    protocol: crate::protocol::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // The agent asks a clarification question; capture the resolved answers.
        let host = server.host().clone();
        let ask = tokio::spawn(async move {
            let mut rx = host
                .ask_clarification(
                    "agent-01",
                    vec![ClarifyQuestion {
                        prompt: "target db?".into(),
                        options: vec!["postgres".into(), "sqlite".into()],
                    }],
                )
                .await
                .unwrap();
            loop {
                if let kontur_mcp::ClarifyDecision::Answered(a) = rx.borrow_and_update().clone() {
                    return a;
                }
                if rx.changed().await.is_err() {
                    return vec![];
                }
            }
        });

        // Wire enters the Clarify phase with the question.
        let clar = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::Clarify { .. })
            }),
        )
        .await
        .expect("Clarify phase");
        if let WirePhase::Clarify { questions } = &clar.phase {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].options, vec!["postgres", "sqlite"]);
            assert!(questions[0].allows_custom);
        } else {
            panic!("expected Clarify phase");
        }

        // Both operators pick option 1 (postgres) → agreement.
        for w in [&mut ca_write, &mut cb_write] {
            write_json(
                w,
                &ClientMsg::Answer {
                    question: 0,
                    choice: crate::protocol::WireChoice::Option(0),
                },
            )
            .await
            .unwrap();
        }

        // The agent's ask future resolves with the agreed answer...
        let answers = tokio::time::timeout(Duration::from_secs(5), ask)
            .await
            .expect("ask resolves")
            .expect("join");
        assert_eq!(answers, vec![vec!["postgres".to_string()]]);

        // ...and the phase returns to PlanReview.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("back to PlanReview after clarification");
    }

    /// A seat's AFK flag toggles on the wire and never affects readiness or
    /// any gate; a disconnect clears it (a returning operator starts present).
    #[tokio::test]
    async fn afk_toggles_on_wire_and_clears_on_disconnect() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        // Keep B's read half so we can drop BOTH halves to force a disconnect.
        let cb_buf = BufReader::new(cb_read);

        for (w, seat, op) in [(&mut ca_write, "A", op1), (&mut cb_write, "B", op2)] {
            write_json(
                w,
                &ClientMsg::Hello {
                    seat: seat.into(),
                    operator: op,
                    protocol: crate::protocol::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.seats.iter().all(|seat| seat.linked)),
        )
        .await
        .expect("both linked");

        // B goes AFK → wire shows B afk, and B's readiness is untouched.
        write_json(&mut cb_write, &ClientMsg::SetAfk { afk: true })
            .await
            .unwrap();
        let afk = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.get(1).map(|x| x.afk).unwrap_or(false)
            }),
        )
        .await
        .expect("B afk visible");
        assert!(!afk.seats[1].ready, "AFK must not touch readiness");
        assert!(!afk.seats[0].afk, "A is not AFK");

        // B drops both halves → server sees EOF → afk clears (present-on-return).
        drop(cb_buf);
        drop(cb_write);
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.get(1).map(|x| !x.linked && !x.afk).unwrap_or(false)
            }),
        )
        .await
        .expect("afk cleared on disconnect");
    }

    /// A discuss note appends to the gate's thread and is projected onto the
    /// wire gate with the author's label.
    #[tokio::test]
    async fn discuss_note_appears_on_gate() {
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
        tokio::spawn(crate::agent::run_agent(agent, server.clone()));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        for (w, seat, op) in [(&mut ca_write, "A", op1), (&mut cb_write, "B", op2)] {
            write_json(
                w,
                &ClientMsg::Hello {
                    seat: seat.into(),
                    operator: op,
                    protocol: crate::protocol::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        let gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate");
        let gid = gate.gate.unwrap().gate_id;

        write_json(
            &mut cb_write,
            &ClientMsg::Discuss {
                gate_id: gid,
                text: "  is this covered by a test?  ".into(),
            },
        )
        .await
        .unwrap();

        let noted = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate
                    .as_ref()
                    .map(|g| !g.discuss.is_empty())
                    .unwrap_or(false)
            }),
        )
        .await
        .expect("discuss note visible");
        let note = &noted.gate.unwrap().discuss[0];
        assert_eq!(note.who, "B");
        assert_eq!(note.text, "is this covered by a test?", "trimmed");
    }

    /// A [c] claim marks the active gate with the claimer's label on the wire,
    /// and a second claim from the same seat releases it.
    #[tokio::test]
    async fn claim_marks_gate_and_toggles_off() {
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
        tokio::spawn(crate::agent::run_agent(agent, server.clone()));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        for (w, seat, op) in [(&mut ca_write, "A", op1), (&mut cb_write, "B", op2)] {
            write_json(
                w,
                &ClientMsg::Hello {
                    seat: seat.into(),
                    operator: op,
                    protocol: crate::protocol::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        }

        // Dispatch through to the gate.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        let gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate opened");
        let gid = gate.gate.unwrap().gate_id;

        // A claims → wire shows A's label.
        write_json(
            &mut ca_write,
            &ClientMsg::Claim {
                gate_id: gid.clone(),
            },
        )
        .await
        .unwrap();
        let claimed = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate.as_ref().and_then(|g| g.claimed_by.as_deref()) == Some("A")
            }),
        )
        .await
        .expect("A's claim visible");
        assert_eq!(claimed.gate.unwrap().claimed_by.as_deref(), Some("A"));

        // A claims again → released.
        write_json(&mut ca_write, &ClientMsg::Claim { gate_id: gid })
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.gate
                    .as_ref()
                    .map(|g| g.claimed_by.is_none())
                    .unwrap_or(false)
            }),
        )
        .await
        .expect("claim released on second press");
    }

    /// A client on a different protocol version is rejected at Hello with a
    /// clear message naming both versions — not an opaque serde error later.
    #[tokio::test]
    async fn protocol_mismatch_rejected_at_hello() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);

        let (client_a, server_a) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION + 99,
            },
        )
        .await
        .unwrap();

        // The server may emit an initial State snapshot before the rejection;
        // read until the Rejected arrives.
        let mut rejected = None;
        for _ in 0..4 {
            match read_json::<_, ServerMsg>(&mut ca_reader).await.unwrap() {
                Some(ServerMsg::Rejected { reason }) => {
                    rejected = Some(reason);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        let reason = rejected.expect("expected a Rejected message");
        assert!(reason.contains("protocol mismatch"), "got: {reason}");
        assert!(reason.contains("update kontur"), "got: {reason}");
    }

    /// A pre-versioning client (Hello without the field) deserializes to
    /// protocol 0 and is likewise rejected — no panic, no serde error.
    #[test]
    fn hello_without_protocol_defaults_to_zero() {
        let json = r#"{"Hello":{"seat":"A","operator":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]}}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Hello { protocol, .. } => assert_eq!(protocol, 0),
            _ => panic!("expected Hello"),
        }
    }

    /// Live prompt sync: a draft keystroke from one seat is visible in the
    /// broadcast state (the other seat sees typing as it happens), resets
    /// both ready flags, and produces no log line — only the SetPrompt
    /// commit is logged. After dispatch, drafts are rejected as locked.
    #[tokio::test]
    async fn prompt_draft_syncs_live_and_is_unlogged() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server_with_prompt(op1, op2, vec!["t1".into()], "");
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // B marks ready; A then types — the draft must reset B's ready flag.
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.seats.iter().any(|seat| seat.ready)),
        )
        .await
        .expect("B ready observed");

        // Keystrokes stream as full-text drafts.
        for draft in ["f", "fi", "fix"] {
            write_json(
                &mut ca_write,
                &ClientMsg::PromptDraft {
                    prompt: draft.into(),
                },
            )
            .await
            .unwrap();
        }

        let synced = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(
                &mut state_rx,
                |s| matches!(&s.phase, WirePhase::DispatchReady { prompt, .. } if prompt == "fix"),
            ),
        )
        .await
        .expect("draft visible in broadcast state");
        assert!(
            synced.seats.iter().all(|seat| !seat.ready),
            "draft must reset both ready flags"
        );
        assert!(
            !synced.log.iter().any(|l| l.contains("prompt")),
            "drafts must not be logged; log: {:?}",
            synced.log
        );

        // Commit + dispatch, then a late draft must be rejected as locked.
        write_json(
            &mut ca_write,
            &ClientMsg::SetPrompt {
                prompt: "fix the thing".into(),
            },
        )
        .await
        .unwrap();
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("dispatched");

        write_json(
            &mut cb_write,
            &ClientMsg::PromptDraft {
                prompt: "too late".into(),
            },
        )
        .await
        .unwrap();
        // The locked rejection goes to B's connection; observable here as the
        // prompt not changing. Give the server a beat, then assert.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = state_rx.borrow().clone();
        assert!(
            !matches!(&after.phase, WirePhase::DispatchReady { .. }),
            "phase must stay past dispatch"
        );
    }

    /// A blank session prompt must not dispatch: both-ready in DispatchReady
    /// is refused (readies reset, reason logged) until a prompt is composed,
    /// after which both-ready advances to PlanReview as normal.
    #[tokio::test]
    async fn dispatch_refused_while_prompt_empty() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server_with_prompt(op1, op2, vec!["t1".into()], "");
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both seats mark ready against the blank prompt.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        // Refusal is observable: readies reset and the reason is logged,
        // while the phase stays DispatchReady.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.log.iter().any(|l| l.contains("prompt is empty"))
            }),
        )
        .await
        .expect("empty-prompt refusal logged");
        assert!(
            matches!(refused.phase, WirePhase::DispatchReady { .. }),
            "phase must stay DispatchReady on blank prompt"
        );
        assert!(
            refused.seats.iter().all(|seat| !seat.ready),
            "both ready flags must reset on refusal"
        );

        // Compose a prompt, re-signal consent → dispatch proceeds.
        write_json(
            &mut ca_write,
            &ClientMsg::SetPrompt {
                prompt: "fix the thing".into(),
            },
        )
        .await
        .unwrap();
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("dispatch proceeds once a prompt exists");
    }

    // -----------------------------------------------------------------------
    // Test: hand_edit_realtime_diff_sync
    //
    // After a HandEdit over the wire (fix: stale pending gate is superseded):
    //   1. The server broadcasts a state update to ALL seats (realtime property).
    //   2. The stale original gate is removed; ONLY the fresh hand-edit gate
    //      remains in pending_gates.
    //   3. BOTH seats' next WireState carries the new gate_id AND a diff_preview
    //      containing the edited content — proving the wire projects the fresh
    //      gate, not the stale one.
    //   4. The hand-edit content is in the workspace immediately.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn hand_edit_realtime_diff_sync() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, ws) = make_server(op1, op2, vec!["guard.rs".into()]);
        let mut state_rx_a = server.state_rx();
        let mut state_rx_b = server.state_rx();

        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/guard.rs".into(),
                contents: "// original\n".into(),
            }],
        };
        tokio::spawn(crate::agent::run_agent(agent, server.clone()));

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Drive through to Executing.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_a, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_a, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_a, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Wait for the first gate (opened by the scripted agent).
        let state_with_gate = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_a, |s| s.gate.is_some()),
        )
        .await
        .expect("first gate");
        let original_gate_id = state_with_gate.gate.as_ref().unwrap().gate_id.clone();

        // A sends HandEdit.
        let edit_contents = "// edited by hand\npub fn guard() { todo!() }\n";
        write_json(
            &mut ca_write,
            &ClientMsg::HandEdit {
                path: "src/guard.rs".into(),
                contents: edit_contents.into(),
            },
        )
        .await
        .unwrap();

        // Realtime property: BOTH seats receive a broadcast where the wire
        // projects the FRESH gate (not the stale original), and the per-file
        // diffs contain the edited content.
        let fresh_gate_check = |s: &WireState| {
            s.gate
                .as_ref()
                .map(|g| {
                    g.gate_id != original_gate_id
                        && g.file_diffs
                            .iter()
                            .any(|fd| fd.diff.contains("edited by hand"))
                })
                .unwrap_or(false)
        };

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_a, fresh_gate_check),
        )
        .await
        .expect("A: wire projects fresh gate with edited content after hand-edit");
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx_b, fresh_gate_check),
        )
        .await
        .expect("B: wire projects fresh gate with edited content after hand-edit");

        // After supersession: ONLY the fresh gate remains pending (stale removed).
        let pending = server.inner.host.pending_gates().await;
        assert_eq!(
            pending.len(),
            1,
            "only the fresh hand-edit gate must remain pending"
        );
        assert_ne!(
            pending[0].gate_id, original_gate_id,
            "fresh gate must have a new id"
        );

        // The workspace holds the hand-edit content immediately.
        let task = kontur_core::TaskId("t1".into());
        let new_bytes = ws
            .file_contents(&task, "src/guard.rs")
            .expect("file must be recorded");
        assert!(
            new_bytes
                .windows(b"edited by hand".len())
                .any(|w| w == b"edited by hand"),
            "workspace must reflect hand-edit content"
        );
    }

    // -----------------------------------------------------------------------
    // Test: fetch_file_roundtrip
    //
    // After writing a file via host.record_write + begin_task_gate, seat A
    // sends FetchFile for that path and receives FileContent with the correct
    // contents. Fetching a missing path returns FileContent { contents: None }.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_file_roundtrip() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        // Write a file directly via the host (simulates MCP agent write).
        let task = kontur_core::TaskId("t1".into());
        server
            .host()
            .record_write("agent-01", &task, "main.rs", b"fn main() {}\n")
            .await
            .unwrap();
        server
            .host()
            .begin_task_gate("agent-01", task.clone(), 0)
            .await
            .unwrap();

        // Attach one client.
        let (client_a, server_a) = tokio::io::duplex(65536);
        server.attach(server_a).await;

        // Split — keep the read side live for FetchFile responses.
        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        let msg_tx2 = msg_tx.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(ca_read);
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut r).await {
                let _ = msg_tx2.send(msg).await;
            }
        });

        // Send Hello (only one seat — that's fine for this test).
        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        // Wait for the gate to be visible in state.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| s.gate.is_some()),
        )
        .await
        .expect("gate visible");

        // --- Test 1: fetch an existing file ---
        write_json(
            &mut ca_write,
            &ClientMsg::FetchFile {
                path: "main.rs".into(),
            },
        )
        .await
        .unwrap();

        let file_content = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match msg_rx.recv().await {
                    Some(ServerMsg::FileContent { path, contents }) => return (path, contents),
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("FileContent for main.rs");

        assert_eq!(file_content.0, "main.rs");
        assert_eq!(
            file_content.1.as_deref(),
            Some("fn main() {}\n"),
            "contents must match what was written"
        );

        // --- Test 2: fetch a missing path → None ---
        write_json(
            &mut ca_write,
            &ClientMsg::FetchFile {
                path: "does_not_exist.rs".into(),
            },
        )
        .await
        .unwrap();

        let missing_content = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match msg_rx.recv().await {
                    Some(ServerMsg::FileContent { path, contents }) => return (path, contents),
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("FileContent for missing path");

        assert_eq!(missing_content.0, "does_not_exist.rs");
        assert!(
            missing_content.1.is_none(),
            "missing path must return None contents"
        );

        // Verify the workspace recorded the write.
        assert_eq!(
            ws.file_contents(&task, "main.rs"),
            Some(b"fn main() {}\n".to_vec())
        );
    }

    // -----------------------------------------------------------------------
    // Plan steer tests
    // -----------------------------------------------------------------------

    /// SteerPlan sent outside PlanReview (Executing phase) must be Rejected
    /// with "plan is locked".
    #[tokio::test]
    async fn steer_plan_outside_plan_review_rejected() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(cb_read)).await;
        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both ready → PlanReview.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Both ready → Executing (config plan is non-empty).
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Drain A's read side into a channel to inspect Rejected.
        let (drain_tx, mut drain_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut ca_reader).await {
                let _ = drain_tx.send(msg).await;
            }
        });

        // SteerPlan while Executing → Rejected "plan is locked".
        write_json(
            &mut ca_write,
            &ClientMsg::SteerPlan {
                steer: "revise".into(),
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, drain_rx.recv()).await {
                Ok(Some(ServerMsg::Rejected { reason })) if reason.contains("locked") => {
                    got = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            got,
            "SteerPlan outside PlanReview must be Rejected with 'locked' in reason"
        );
    }

    /// SteerPlan with an empty/whitespace steer during PlanReview must be
    /// Rejected with "steer cannot be empty".
    #[tokio::test]
    async fn empty_steer_rejected() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["t1".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(cb_read)).await;
        let mut ca_reader = BufReader::new(ca_read);

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        let (drain_tx, mut drain_rx) = tokio::sync::mpsc::channel::<ServerMsg>(64);
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut ca_reader).await {
                let _ = drain_tx.send(msg).await;
            }
        });

        // Empty steer during PlanReview → Rejected.
        write_json(
            &mut ca_write,
            &ClientMsg::SteerPlan {
                steer: "   ".into(),
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, drain_rx.recv()).await {
                Ok(Some(ServerMsg::Rejected { reason })) if reason.contains("empty") => {
                    got = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            got,
            "empty SteerPlan must be Rejected with 'empty' in reason"
        );
    }

    /// A steer during PlanReview resets both ready flags and withdraws the
    /// current plan list (agent_plan → None on the wire).
    #[tokio::test]
    async fn steer_plan_resets_ready_and_clears_plan() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["config-task".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both ready → PlanReview.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Now that the session is live (event pump definitely subscribed), the
        // agent proposes a plan — the pump projects it onto the wire.
        server
            .inner
            .host
            .propose_plan(
                "agent-01",
                vec!["agent-task-1".into(), "agent-task-2".into()],
            )
            .await
            .unwrap();

        // Wait for the agent plan to project onto the wire.
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(&s.phase, WirePhase::PlanReview { tasks } if tasks.contains(&"agent-task-1".to_string()))
        })).await.expect("agent plan on wire");

        // Seat A marks ready.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                s.seats.iter().any(|st| st.label == "A" && st.ready)
            }),
        )
        .await
        .expect("A ready");

        // Seat B sends a steer.
        write_json(
            &mut cb_write,
            &ClientMsg::SteerPlan {
                steer: "split task 2".into(),
            },
        )
        .await
        .unwrap();

        // State must show: A ready=false and the plan withdrawn (agent_plan None →
        // wire falls back to the config plan, which does NOT contain agent-task-1).
        let after = tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            let a_ready = s.seats.iter().find(|st| st.label == "A").map(|st| st.ready).unwrap_or(true);
            let plan_cleared = matches!(&s.phase, WirePhase::PlanReview { tasks } if !tasks.contains(&"agent-task-1".to_string()));
            !a_ready && plan_cleared
        })).await.expect("A reset and plan withdrawn after steer");

        // Explicit assertions.
        let a_ready = after
            .seats
            .iter()
            .find(|st| st.label == "A")
            .map(|st| st.ready)
            .unwrap();
        assert!(!a_ready, "seat A ready must reset after steer");
        match &after.phase {
            WirePhase::PlanReview { tasks } => {
                assert!(
                    !tasks.contains(&"agent-task-1".to_string()),
                    "agent plan must be withdrawn after steer"
                );
            }
            other => panic!("expected PlanReview, got {other:?}"),
        }
        // The steer was routed to the host: proposed_plan is unchanged (steer does
        // not overwrite the stored plan), but the decision channel is Steered.
        assert!(server.inner.host.proposed_plan().await.is_some());
    }

    // -----------------------------------------------------------------------
    // Consent-integrity regression tests (fix: both-ready arm is the single
    // sync point for set_plan; EditPlan's set_plan was removed).
    // -----------------------------------------------------------------------

    /// loopback: propose via host, EditPlan to a modified list, both ready →
    /// host.proposed_plan() equals the EDITED list right after the Executing
    /// transition. Proves the both-ready arm calls set_plan(effective_plan)
    /// before approve_plan, so the agent receives what both seats signed.
    #[tokio::test]
    async fn consent_integrity_editplan_host_proposed_plan_matches_edited_list() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        // Start with a config plan that will be overridden by the agent proposal.
        let (server, _ws) = make_server(op1, op2, vec!["config-only".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both ready → PlanReview.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Agent proposes a plan via the host (simulates propose_plan MCP call).
        server
            .inner
            .host
            .propose_plan("agent-01", vec!["task-alpha".into(), "task-beta".into()])
            .await
            .unwrap();

        // Wait for the agent plan to appear on the wire.
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(&s.phase, WirePhase::PlanReview { tasks } if tasks.contains(&"task-alpha".to_string()))
        })).await.expect("agent plan on wire");

        // Seat A edits: replaces the list with a modified version.
        let edited = vec!["task-beta".into(), "task-alpha-modified".into()];
        write_json(
            &mut ca_write,
            &ClientMsg::EditPlan {
                tasks: edited.clone(),
            },
        )
        .await
        .unwrap();

        // Wait for the edit to propagate (both seats reset to not-ready).
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(&s.phase, WirePhase::PlanReview { tasks } if tasks.contains(&"task-alpha-modified".to_string()))
                && s.seats.iter().all(|st| !st.ready)
        })).await.expect("edited plan on wire");

        // Both seats approve the edited plan.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // KEY ASSERTION: host.proposed_plan() must equal the EDITED list, not the
        // original agent proposal. The both-ready arm called set_plan(effective_plan)
        // under the net lock before approve_plan.
        let stored = server.inner.host.proposed_plan().await;
        assert_eq!(
            stored,
            Some(edited),
            "host.proposed_plan() must equal the edited list that both seats signed; got {stored:?}"
        );
    }

    /// fallback path: no propose_plan (scripted-style cfg.plan), both ready →
    /// host.proposed_plan() == cfg.plan (not None/empty). Proves the both-ready
    /// arm calls set_plan even when agent_plan is None (the cfg.plan fallback).
    #[tokio::test]
    async fn consent_integrity_fallback_cfg_plan_stored_in_host() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let cfg_plan = vec!["scripted-task-1".into(), "scripted-task-2".into()];
        let (server, _ws) = make_server(op1, op2, cfg_plan.clone());
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        // Both ready → PlanReview (no propose_plan call — using cfg.plan fallback).
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Confirm no agent plan was proposed (host.proposed_plan is None at this point).
        assert_eq!(
            server.inner.host.proposed_plan().await,
            None,
            "no agent proposal expected before approval"
        );

        // Both ready → Executing (cfg.plan is non-empty, so this transitions).
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // KEY ASSERTION: the both-ready arm must have pushed cfg.plan into the host
        // so the agent always sees a non-None, non-empty plan — never a surprise [].
        let stored = server.inner.host.proposed_plan().await;
        assert_eq!(
            stored,
            Some(cfg_plan.clone()),
            "host.proposed_plan() must equal cfg.plan after fallback approval; got {stored:?}"
        );
    }

    /// consent-integrity wire alignment: agent proposes [a, b]; a seat EditPlans
    /// to [b, a-modified]; both approve; the wire's PlanReview tasks immediately
    /// before the Executing transition contain the edited list [b, a-modified].
    /// This is the wire-level confirmation that the wire and host are in sync.
    #[tokio::test]
    async fn consent_integrity_wire_tasks_match_editplan_before_executing() {
        let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
        let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();

        let (server, _ws) = make_server(op1, op2, vec!["config-fallback".into()]);
        let mut state_rx = server.state_rx();

        let (client_a, server_a) = tokio::io::duplex(65536);
        let (client_b, server_b) = tokio::io::duplex(65536);
        server.attach(server_a).await;
        server.attach(server_b).await;

        let (ca_read, mut ca_write) = tokio::io::split(client_a);
        let (cb_read, mut cb_write) = tokio::io::split(client_b);
        drain_client(BufReader::new(ca_read)).await;
        drain_client(BufReader::new(cb_read)).await;

        write_json(
            &mut ca_write,
            &ClientMsg::Hello {
                seat: "A".into(),
                operator: op1,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        write_json(
            &mut cb_write,
            &ClientMsg::Hello {
                seat: "B".into(),
                operator: op2,
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::DispatchReady { .. })
            }),
        )
        .await
        .expect("DispatchReady");

        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| {
                matches!(s.phase, WirePhase::PlanReview { .. })
            }),
        )
        .await
        .expect("PlanReview");

        // Agent proposes [a, b].
        server
            .inner
            .host
            .propose_plan("agent-01", vec!["a".into(), "b".into()])
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), wait_for_state(&mut state_rx, |s| {
            matches!(&s.phase, WirePhase::PlanReview { tasks } if tasks.contains(&"a".to_string()))
        })).await.expect("agent plan [a, b] on wire");

        // Seat A edits to [b, a-modified].
        let edited = vec!["b".into(), "a-modified".into()];
        write_json(
            &mut ca_write,
            &ClientMsg::EditPlan {
                tasks: edited.clone(),
            },
        )
        .await
        .unwrap();

        // Wait for edit to land on the wire.
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(
                &mut state_rx,
                |s| matches!(&s.phase, WirePhase::PlanReview { tasks } if tasks == &edited),
            ),
        )
        .await
        .expect("edited list on wire");

        // Capture the last PlanReview state before approval.
        let pre_approval = state_rx.borrow().clone();
        let pre_tasks = match &pre_approval.phase {
            WirePhase::PlanReview { tasks } => tasks.clone(),
            other => panic!("expected PlanReview, got {other:?}"),
        };
        assert_eq!(
            pre_tasks, edited,
            "wire tasks before approval must be the edited list"
        );

        // Both approve.
        write_json(&mut ca_write, &ClientMsg::Ready).await.unwrap();
        write_json(&mut cb_write, &ClientMsg::Ready).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_state(&mut state_rx, |s| matches!(s.phase, WirePhase::Executing)),
        )
        .await
        .expect("Executing");

        // Wire-level + host-level both agree on the edited list.
        let stored = server.inner.host.proposed_plan().await;
        assert_eq!(
            stored,
            Some(edited.clone()),
            "host.proposed_plan() must equal edited list; got {stored:?}"
        );
    }
}
