//! Remote two-seat mode: connects to a kontur-net SessionServer over TCP,
//! maps WireState → SessionView, and runs the interactive terminal loop.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use kontur_core::{GateId, OperatorId, ReviewDepth, Verdict, VerdictStatus};
use kontur_net::{ServerMsg, SessionClient, WireGate, WirePhase, WireRole, WireState};

use crate::app::{poll_action, TerminalGuard};
use crate::compose;
use crate::diffview::{clamp_scroll, editor_command};
use crate::input::Action;
use crate::planedit;
use crate::render::render;
use crate::view::{
    ActiveRegion, AgentCard, Attention, AuditSummary, Banner, GateCard, KeyStatus, KeyView,
    LogLine, Role, SessionView, Station, StatusStrip,
};

// ---------------------------------------------------------------------------
// Compose-state machine (local to the loop)
// ---------------------------------------------------------------------------

enum ComposeTarget {
    None,
    Remedy,
    ConfirmAbandon,
    Prompt,
    /// Editing a plan task in-place. `idx` is the task's index in the list.
    PlanEdit {
        idx: usize,
    },
    /// Composing a plan steer prompt.
    PlanSteer,
    /// Composing a gate discussion note.
    Discuss {
        gate_id: String,
    },
    /// Composing a custom answer to a clarification question.
    ClarifyCustom {
        question: usize,
    },
}

// ---------------------------------------------------------------------------
// wire_to_view
// ---------------------------------------------------------------------------

/// Map a WireState snapshot to a pure SessionView. The `own` id is used to
/// compute `needs_you` and is not exposed in the rendered output.
/// `plan_sel` is the currently highlighted row in PlanReview — it is loop-local
/// state (not from the wire) so it's passed in explicitly.
pub fn wire_to_view(state: &WireState, own: OperatorId, plan_sel: usize) -> SessionView {
    // --- stations ---
    let stations: [Station; 2] = {
        let mut iter = state.seats.iter();
        let make = |ws: &kontur_net::WireSeat| Station {
            label: ws.label.clone(),
            role: match ws.role {
                WireRole::Host => Role::Host,
                WireRole::Operator => Role::Operator,
            },
            activity: if ws.linked {
                "linked".into()
            } else {
                "dropped".into()
            },
            operator: ws.operator,
            afk: ws.afk,
        };
        // Guarantee exactly 2 stations; pad with a placeholder if needed.
        let a = iter.next();
        let b = iter.next();
        match (a, b) {
            (Some(a), Some(b)) => [make(a), make(b)],
            (Some(a), None) => [
                make(a),
                Station {
                    label: "B".into(),
                    role: Role::Operator,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                    afk: false,
                },
            ],
            _ => [
                Station {
                    label: "A".into(),
                    role: Role::Host,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                    afk: false,
                },
                Station {
                    label: "B".into(),
                    role: Role::Operator,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                    afk: false,
                },
            ],
        }
    };

    // --- fleet ---
    let fleet: Vec<AgentCard> = state
        .fleet
        .iter()
        .map(|f| AgentCard {
            id: f.id.clone(),
            status: f.status.clone(),
            needs_signoff: f.needs_signoff,
        })
        .collect();

    // --- log ---
    let log: Vec<LogLine> = state
        .log
        .iter()
        .map(|l| LogLine {
            time: String::new(),
            who: String::new(),
            text: l.clone(),
        })
        .collect();

    // --- status strip ---
    let both_linked = state.seats.iter().all(|s| s.linked);
    let fleet_count = fleet.len();

    // needs_you: count pending gates (gate present + own key not yet in keys)
    let needs_you = if let Some(gate) = &state.gate {
        let own_has_key = gate.keys.iter().any(|k| k.operator == own);
        if own_has_key {
            0
        } else {
            1
        }
    } else {
        0
    };

    let status = StatusStrip {
        linked: both_linked,
        four_eyes: true,
        fleet_count,
        needs_you,
    };

    // --- active region ---
    let active = match &state.phase {
        WirePhase::AwaitOperators => ActiveRegion::Idle,
        WirePhase::DispatchReady { prompt } => {
            let ready = [
                state.seats.first().map(|s| s.ready).unwrap_or(false),
                state.seats.get(1).map(|s| s.ready).unwrap_or(false),
            ];
            ActiveRegion::Prompt {
                prompt: prompt.clone(),
                ready,
            }
        }
        WirePhase::PlanReview { tasks } => {
            let ready = [
                state.seats.first().map(|s| s.ready).unwrap_or(false),
                state.seats.get(1).map(|s| s.ready).unwrap_or(false),
            ];
            ActiveRegion::Plan {
                tasks: tasks.clone(),
                ready,
                selected: plan_sel,
            }
        }
        WirePhase::Clarify { questions } => {
            let own_idx = state
                .seats
                .iter()
                .position(|s| s.operator == own)
                .unwrap_or(0);
            ActiveRegion::Clarify {
                questions: questions
                    .iter()
                    .map(|q| crate::view::ClarifyQ {
                        prompt: q.prompt.clone(),
                        options: q.options.clone(),
                        allows_custom: q.allows_custom,
                        picks: q.picks.clone(),
                        resolved: q.resolved.clone(),
                    })
                    .collect(),
                selected: plan_sel,
                own: own_idx,
            }
        }
        WirePhase::Executing => {
            if let Some(wg) = &state.gate {
                ActiveRegion::Gate(wire_gate_to_card(wg, &stations))
            } else {
                ActiveRegion::Idle
            }
        }
        WirePhase::Closed {
            gates,
            chain_verified,
            reviewers,
            merged,
            abandoned,
        } => ActiveRegion::SessionClosed(AuditSummary {
            gates: *gates,
            chain_verified: *chain_verified,
            reviewers: reviewers.clone(),
            merged: *merged,
            abandoned: *abandoned,
        }),
    };

    // The instruction stays on screen after dispatch (plan review + execution);
    // while composing at the dispatch gate the PROMPT pane shows the draft, so
    // no separate TASK line is needed there.
    let instruction = match &state.phase {
        WirePhase::PlanReview { .. } | WirePhase::Executing if !state.prompt.is_empty() => {
            Some(state.prompt.clone())
        }
        _ => None,
    };

    SessionView {
        banner: Banner {
            session: "remote".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        status,
        stations,
        fleet,
        log,
        active,
        invite: None,
        notice: None,
        attention: None,
        instruction,
        show_help: false,
        agent_log: None,
        link_lost: false,
        cursor: None,
        blink_on: false,
        join_request: None,
    }
}

// ---------------------------------------------------------------------------
// attention_for — pure, per-seat attention computation
// ---------------------------------------------------------------------------

/// Compute the attention line for the operator at `own` seat from the current
/// wire state. Returns `Some(Attention { text, loud })` when a line should be
/// shown, or `None` when the seat is calm (fleet/log already show activity).
///
/// Rules:
/// - DispatchReady, own not ready  → loud "confirm the prompt"
/// - DispatchReady, own ready, other not → calm "waiting on <other>"
/// - PlanReview, tasks empty       → calm "waiting on agent's plan"
/// - PlanReview, tasks present, own not ready → loud "review the plan"
/// - PlanReview, own ready, other not → calm "waiting on <other>"
/// - Executing with gate, own key absent → loud "review the diff and cast"
/// - Executing with gate, own key present (sealed), other absent → calm "sealed — waiting on <other>"
/// - Executing no gate             → None
/// - Closed / AwaitOperators       → None
pub fn attention_for(state: &WireState, own: OperatorId) -> Option<Attention> {
    // Helpers: seat index for `own`, label of the other seat (AFK-annotated).
    let own_seat_idx = state.seats.iter().position(|s| s.operator == own);
    let other_label = |own_idx: usize| -> String {
        state
            .seats
            .iter()
            .enumerate()
            .find(|(i, _)| *i != own_idx)
            .map(|(_, s)| {
                if s.afk {
                    format!("{} (AFK)", s.label)
                } else {
                    s.label.clone()
                }
            })
            .unwrap_or_else(|| "other".into())
    };

    // If THIS seat is AFK, the console is unattended — show a single calm line
    // so resuming is one keypress; nothing else needs shouting at an empty chair.
    if let Some(idx) = own_seat_idx {
        if state.seats.get(idx).map(|s| s.afk).unwrap_or(false) {
            return Some(Attention {
                text: "you are AFK — press [z] to resume".into(),
                loud: false,
            });
        }
    }

    match &state.phase {
        WirePhase::DispatchReady { .. } => {
            let own_idx = own_seat_idx?;
            let own_ready = state.seats.get(own_idx).map(|s| s.ready).unwrap_or(false);
            if !own_ready {
                Some(Attention {
                    text: "▶ ACTION: confirm the prompt — [y] ready · [p] edit".into(),
                    loud: true,
                })
            } else {
                // Own is ready; check whether the other seat is also ready.
                let other_ready = state
                    .seats
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != own_idx)
                    .all(|(_, s)| s.ready);
                if !other_ready {
                    Some(Attention {
                        text: format!("waiting on {} to confirm", other_label(own_idx)),
                        loud: false,
                    })
                } else {
                    // Both ready — dispatch is imminent; no extra line needed.
                    None
                }
            }
        }

        WirePhase::PlanReview { tasks } => {
            let own_idx = own_seat_idx?;
            if tasks.is_empty() {
                return Some(Attention {
                    text: "waiting on the agent's plan".into(),
                    loud: false,
                });
            }
            let own_ready = state.seats.get(own_idx).map(|s| s.ready).unwrap_or(false);
            if !own_ready {
                Some(Attention {
                    text: "▶ ACTION: review the plan — [y] approve · [r] steer".into(),
                    loud: true,
                })
            } else {
                let other_ready = state
                    .seats
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != own_idx)
                    .all(|(_, s)| s.ready);
                if !other_ready {
                    Some(Attention {
                        text: format!("waiting on {}", other_label(own_idx)),
                        loud: false,
                    })
                } else {
                    None
                }
            }
        }

        WirePhase::Executing => {
            let gate = state.gate.as_ref()?;
            // Is own key present in the gate's key list?
            let own_key = gate.keys.iter().find(|k| k.operator == own);
            if own_key.is_none() {
                // Own has not cast yet — must act.
                Some(Attention {
                    text: "▶ ACTION: review the diff and cast — [g] go · [r] no-go".into(),
                    loud: true,
                })
            } else {
                // Own key is present (sealed or revealed). Check other seat.
                let own_idx = own_seat_idx?;
                let other_has_key = gate.keys.iter().any(|k| k.operator != own);
                if !other_has_key {
                    Some(Attention {
                        text: format!("your key is sealed — waiting on {}", other_label(own_idx)),
                        loud: false,
                    })
                } else {
                    // Both keys present — resolution is imminent; no line needed.
                    None
                }
            }
        }

        WirePhase::Clarify { questions } => {
            let own_idx = own_seat_idx?;
            let owed = questions
                .iter()
                .any(|q| q.resolved.is_none() && q.picks[own_idx].is_none());
            if owed {
                Some(Attention {
                    text:
                        "▶ ACTION: the agent needs clarification — answer with [1-9] · [a] custom"
                            .into(),
                    loud: true,
                })
            } else if questions.iter().any(|q| q.resolved.is_none()) {
                Some(Attention {
                    text: format!("waiting on {} to answer", other_label(own_idx)),
                    loud: false,
                })
            } else {
                None
            }
        }

        // No attention needed.
        WirePhase::AwaitOperators | WirePhase::Closed { .. } => None,
    }
}

fn wire_gate_to_card(wg: &WireGate, stations: &[Station; 2]) -> GateCard {
    let keys = stations
        .iter()
        .map(|st| {
            let status = wg
                .keys
                .iter()
                .find(|k| k.operator == st.operator)
                .map(|k| match &k.status {
                    VerdictStatus::Sealed => KeyStatus::Sealed,
                    VerdictStatus::Revealed(Verdict::Go) => KeyStatus::Go,
                    VerdictStatus::Revealed(Verdict::NoGo(_)) => KeyStatus::NoGo,
                })
                .unwrap_or(KeyStatus::Awaiting);
            KeyView {
                label: st.label.clone(),
                role: st.role,
                status,
            }
        })
        .collect();

    GateCard {
        gate_id: wg.gate_id.0.clone(),
        task: wg.task.clone(),
        files: wg.files.clone(),
        loc: wg.loc,
        keys,
        escalation_required: wg.escalation_required,
        file_diffs: wg
            .file_diffs
            .iter()
            .map(|fd| crate::view::FileDiffView {
                path: fd.path.clone(),
                diff: fd.diff.clone(),
                truncated: fd.truncated,
            })
            .collect(),
        diff_truncated: wg.diff_truncated,
        last_cmd: wg
            .last_cmd
            .as_ref()
            .map(|c| (c.command.clone(), c.exit_code)),
        claimed_by: wg.claimed_by.clone(),
        discuss: wg
            .discuss
            .iter()
            .map(|c| (c.who.clone(), c.text.clone()))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// run_remote
// ---------------------------------------------------------------------------

/// Connect to a kontur-net session server, enter the TUI, and loop until quit.
/// Which invite flavour the host console is currently showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkMode {
    Lan,
    Wan,
}

/// Compose the invite panel text for the current mode. Pure; tested.
/// Falls back to whichever flavour exists when the preferred one is absent.
pub fn compose_invite_text(links: &crate::link::InviteLinks, mode: LinkMode) -> Option<String> {
    let (primary, alt_hint) = match mode {
        LinkMode::Lan => (
            links.lan.as_ref().or(links.wan.as_ref()),
            links.wan.is_some() && links.lan.is_some(),
        ),
        LinkMode::Wan => (
            links.wan.as_ref().or(links.lan.as_ref()),
            links.lan.is_some() && links.wan.is_some(),
        ),
    };
    let primary = primary?;
    let mut text = primary.clone();
    let effective_wan = matches!(mode, LinkMode::Wan) && links.wan.is_some();
    if effective_wan {
        text.push_str(&format!(
            "\nWAN link — forward port {} on your router first",
            links.port
        ));
    }
    if alt_hint {
        text.push_str(match mode {
            LinkMode::Lan => "\n[l] switch to WAN link (for an operator off your network)",
            LinkMode::Wan => "\n[l] switch to LAN link (same machine or network)",
        });
    }
    Some(text)
}

// ---------------------------------------------------------------------------
// Page size constant
// ---------------------------------------------------------------------------

const PAGE_LINES: u16 = 20;

pub async fn run_remote(
    addr: &str,
    seat: String,
    seed: [u8; 32],
    invite: Option<crate::link::InviteLinks>,
    fingerprint: Option<[u8; 16]>,
    agent_log: Option<String>,
) -> io::Result<()> {
    let (client, mut rx) = match fingerprint {
        Some(fp) => SessionClient::connect_pinned_tls(addr, seat, seed, fp).await?,
        None => SessionClient::connect_tcp_plain(addr, seat, seed).await?,
    };
    let own = client.operator();
    let mut link_mode = LinkMode::Lan;

    // Fold the mpsc stream into a watch so the render loop always has the
    // latest state without blocking.
    let initial = WireState {
        phase: WirePhase::AwaitOperators,
        seats: vec![],
        fleet: vec![],
        log: vec![],
        gate: None,
        prompt: String::new(),
        pending_join: None,
    };
    let (state_tx, state_rx) = watch::channel(initial);

    // Track transient rejection reason.
    let (rej_tx, mut rej_rx) = mpsc::channel::<String>(4);

    // Dedicated channel for FileContent responses.
    let (file_tx, mut file_rx) = mpsc::channel::<(String, Option<String>)>(4);

    // Flipped to false when the server channel closes (host gone / keepalive
    // timeout), so the UI can raise the loud HOST LOST banner.
    let connected = Arc::new(AtomicBool::new(true));
    let connected_rx = connected.clone();

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                ServerMsg::State(ws) => {
                    let _ = state_tx.send(*ws);
                }
                ServerMsg::Rejected { reason } => {
                    let _ = rej_tx.send(reason).await;
                }
                ServerMsg::Welcome { .. } => {}
                ServerMsg::FileContent { path, contents } => {
                    let _ = file_tx.send((path, contents)).await;
                }
                // Keepalive reply: liveness only, nothing to render. Its arrival
                // already kept the reader task's stream alive.
                ServerMsg::Pong => {}
                // BYO operator: the host is reviewing our key. The dedicated
                // awaiting-approval screen is wired in the next part; for now
                // this is inert (no live path reaches it yet).
                ServerMsg::AwaitingApproval { .. } => {}
            }
        }
        // The reader task closed its side: the host is gone.
        connected.store(false, Ordering::Relaxed);
    });

    let (_guard, mut terminal) = TerminalGuard::enter()?;

    // Boot card: identity, version, provenance — then the console takes over.
    terminal.draw(|f| crate::boot::render_boot(f, env!("CARGO_PKG_VERSION")))?;
    tokio::time::sleep(Duration::from_millis(crate::boot::BOOT_HOLD_MS)).await;

    let mut compose = ComposeTarget::None;
    let mut compose_buf = String::new();
    let mut compose_cursor: usize = 0;
    // Slow-flash cadence for the text-entry caret (~600ms at 200ms poll).
    let mut blink_frame: u32 = 0;
    // Prompt text as it was when [p] was pressed — Esc restores it (drafts
    // stream live to the other seat, so cancel must undo what they saw).
    let mut prompt_before = String::new();
    let mut diff_scroll: u16 = 0;
    let mut log_scroll: usize = 0;
    let mut show_help = false;
    let mut selected_file: usize = 0;
    let mut last_gate_id: Option<String> = None;
    let mut rejected_msg: Option<String> = None;
    let mut rejected_ttl: u8 = 0;
    // Truncation acknowledgment: when the active gate's diff is truncated,
    // the first `g` press sets this to the gate id; the second `g` casts.
    let mut truncation_ack: Option<String> = None;
    // Plan review: currently highlighted task row.
    let mut plan_sel: usize = 0;

    loop {
        // Pick up any new rejection message.
        while let Ok(r) = rej_rx.try_recv() {
            rejected_msg = Some(r);
            rejected_ttl = 30; // ~6 seconds at 200ms
        }
        if rejected_ttl > 0 {
            rejected_ttl -= 1;
        } else {
            rejected_msg = None;
        }

        blink_frame = blink_frame.wrapping_add(1);
        let blink_on = (blink_frame / 3).is_multiple_of(2);
        let state = state_rx.borrow().clone();
        // Clamp plan_sel whenever the task list changes (remote edits can shrink it).
        if let WirePhase::PlanReview { tasks } = &state.phase {
            plan_sel = planedit::clamp_sel(plan_sel, tasks.len());
        }
        let mut view = wire_to_view(&state, own, plan_sel);
        view.attention = attention_for(&state, own);
        view.show_help = show_help;
        view.agent_log = agent_log.clone();
        view.link_lost = !connected_rx.load(Ordering::Relaxed);
        // Host-only: surface a pending BYO join for approval. The host is the
        // seat whose operator matches state.seats[0].
        let is_host = state
            .seats
            .first()
            .map(|s| s.operator == own)
            .unwrap_or(false);
        view.join_request = if is_host {
            state.pending_join.as_ref().map(|p| p.fingerprint.clone())
        } else {
            None
        };
        // The invite is decision-relevant only while the stations are not both
        // linked; the moment they are, it disappears (calm default).
        if !view.status.linked {
            view.invite = invite
                .as_ref()
                .and_then(|l| compose_invite_text(l, link_mode));
        }

        let active_gate_id = state.gate.as_ref().map(|g| g.gate_id.0.clone());

        // Reset scroll and selected file when a new gate arrives.
        if active_gate_id != last_gate_id {
            diff_scroll = 0;
            selected_file = 0;
            truncation_ack = None;
            last_gate_id = active_gate_id.clone();
        }

        // Transient notice: while ttl > 0 the rejection/confirm message is
        // shown on the command row inside the TUI (never via eprintln).
        if rejected_ttl > 0 {
            view.notice = rejected_msg.clone();
        }
        // Compose display: every compose mode renders its buffer on the
        // notice row (exhaustive match — a mode without a display is a
        // compile error, not an invisible input box). Any active rejection
        // stays visible alongside the draft so refusals are never silent.
        let warn = if rejected_ttl > 0 {
            rejected_msg
                .as_deref()
                .map(|m| format!(" · {m}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        view.blink_on = blink_on;
        if let Some((text, caret_col)) =
            compose_notice(&compose, &compose_buf, compose_cursor, &warn)
        {
            view.notice = Some(text);
            // In-notice caret: " > " (3) + column within the notice string.
            if let Some(col) = caret_col {
                view.cursor = Some(crate::view::CursorTarget::Command {
                    col: (col + 3) as u16,
                });
            }
        }
        // Prompt compose: echo the local draft into the PROMPT pane (zero-lag,
        // so the caret index matches) and place the caret there.
        if matches!(compose, ComposeTarget::Prompt) {
            if let ActiveRegion::Prompt { prompt, .. } = &mut view.active {
                *prompt = compose_buf.clone();
            }
            view.cursor = Some(crate::view::CursorTarget::Prompt {
                index: compose_cursor,
            });
        }

        // When a gate is pending with multiple files, show file-cycle hint in notice.
        if view.notice.is_none() {
            if let ActiveRegion::Gate(ref card) = view.active {
                if card.file_diffs.len() > 1 {
                    let path = card
                        .file_diffs
                        .get(selected_file)
                        .map(|fd| fd.path.as_str())
                        .unwrap_or("");
                    view.notice = Some(format!("[tab] file: {path}"));
                }
            }
        }

        terminal.draw(|f| {
            render(f, &view, diff_scroll, selected_file, log_scroll);
        })?;

        let composing = !matches!(compose, ComposeTarget::None);
        let in_plan_review = matches!(view.active, ActiveRegion::Plan { .. }) && !composing;
        let in_clarify = matches!(view.active, ActiveRegion::Clarify { .. }) && !composing;
        match poll_action(
            Duration::from_millis(200),
            composing,
            in_plan_review,
            in_clarify,
        )? {
            None => {}
            Some(Action::Quit) => break,

            // Ready signal (dispatch / plan approval).
            Some(Action::Help) => {
                show_help = !show_help;
            }

            Some(Action::Ready) => {
                let _ = client.ready().await;
            }

            // Plan selection navigation.
            Some(Action::PlanSelectDown) => {
                if let ActiveRegion::Plan { tasks, .. } = &view.active {
                    plan_sel = planedit::clamp_sel(plan_sel.saturating_add(1), tasks.len());
                }
            }
            Some(Action::PlanSelectUp) => {
                plan_sel = plan_sel.saturating_sub(1);
            }

            // Begin composing a plan steer prompt.
            Some(Action::PlanSteerBegin) => {
                if matches!(view.active, ActiveRegion::Plan { .. }) {
                    compose = ComposeTarget::PlanSteer;
                    compose_buf.clear();
                }
            }

            // Begin editing the selected plan task (seeded with current text).
            Some(Action::PlanEditBegin) => {
                if let ActiveRegion::Plan { tasks, .. } = &view.active {
                    let seed = tasks.get(plan_sel).cloned().unwrap_or_default();
                    compose = ComposeTarget::PlanEdit { idx: plan_sel };
                    compose_buf = seed;
                }
            }

            // Delete the selected task (refuse if it would empty the list).
            Some(Action::PlanDeleteTask) => {
                if let ActiveRegion::Plan { tasks, .. } = &view.active {
                    match planedit::delete_task(tasks.clone(), plan_sel) {
                        Ok(new_list) => {
                            plan_sel = planedit::clamp_sel(plan_sel, new_list.len());
                            let _ = client.edit_plan(&new_list).await;
                        }
                        Err(msg) => {
                            rejected_msg = Some(msg.into());
                            rejected_ttl = 30;
                        }
                    }
                }
            }

            // Move selected task up.
            Some(Action::PlanMoveUp) => {
                if let ActiveRegion::Plan { tasks, .. } = &view.active {
                    let (new_list, new_idx) = planedit::move_task(tasks.clone(), plan_sel, true);
                    plan_sel = new_idx;
                    let _ = client.edit_plan(&new_list).await;
                }
            }

            // Move selected task down.
            Some(Action::PlanMoveDown) => {
                if let ActiveRegion::Plan { tasks, .. } = &view.active {
                    let (new_list, new_idx) = planedit::move_task(tasks.clone(), plan_sel, false);
                    plan_sel = new_idx;
                    let _ = client.edit_plan(&new_list).await;
                }
            }

            // Go verdict — truncation requires a second `g` to acknowledge.
            Some(Action::Go) => {
                if let Some(wg) = &state.gate {
                    let acked = truncation_ack.as_deref() == Some(&wg.gate_id.0);
                    match go_gate(wg.diff_truncated, acked) {
                        GoDecision::Cast => {
                            let _ = client.cast_go(wg, ReviewDepth::FullDiff).await;
                            truncation_ack = None;
                        }
                        GoDecision::NeedAck => {
                            truncation_ack = Some(wg.gate_id.0.clone());
                            rejected_msg = Some(
                                "a file diff was truncated at 64 KB — press [g] again to sign anyway"
                                    .into(),
                            );
                            rejected_ttl = 60;
                        }
                    }
                }
            }

            // No-go → start remedy compose.
            Some(Action::NoGoBegin) => {
                compose = ComposeTarget::Remedy;
                compose_buf.clear();
                compose_cursor = 0;
            }

            // Prompt edit → start composing (valid only in DispatchReady region).
            Some(Action::PromptBegin) => {
                if let ActiveRegion::Prompt { prompt, .. } = &view.active {
                    compose = ComposeTarget::Prompt;
                    // Seed with the current prompt so small edits don't require
                    // retyping the whole instruction (same idiom as task editing).
                    // Draft is shown via the notice row while composing.
                    compose_buf = prompt.clone();
                    compose_cursor = compose::end(&compose_buf);
                    prompt_before = prompt.clone();
                }
            }

            // Abandon → request confirmation.
            // Open a discussion-note compose for the active gate.
            Some(Action::Discuss) => {
                if let Some(wg) = &state.gate {
                    compose = ComposeTarget::Discuss {
                        gate_id: wg.gate_id.0.clone(),
                    };
                    compose_buf.clear();
                    compose_cursor = 0;
                }
            }

            // Clarify navigation + answering.
            Some(Action::ClarifyNext) => {
                if let ActiveRegion::Clarify { questions, .. } = &view.active {
                    plan_sel = planedit::clamp_sel(plan_sel.saturating_add(1), questions.len());
                }
            }
            Some(Action::ClarifyPrev) => {
                plan_sel = plan_sel.saturating_sub(1);
            }
            Some(Action::ClarifyDigit(n)) => {
                if let ActiveRegion::Clarify { questions, .. } = &view.active {
                    if let Some(q) = questions.get(plan_sel) {
                        let n = n as usize; // 1-based
                        if n >= 1 && n <= q.options.len() {
                            let _ = client
                                .answer(plan_sel, kontur_net::WireChoice::Option(n - 1))
                                .await;
                        } else if q.allows_custom && n == q.options.len() + 1 {
                            compose = ComposeTarget::ClarifyCustom { question: plan_sel };
                            compose_buf.clear();
                            compose_cursor = 0;
                        }
                    }
                }
            }
            Some(Action::ClarifyCustomBegin) => {
                if let ActiveRegion::Clarify { questions, .. } = &view.active {
                    if questions
                        .get(plan_sel)
                        .map(|q| q.allows_custom)
                        .unwrap_or(false)
                    {
                        compose = ComposeTarget::ClarifyCustom { question: plan_sel };
                        compose_buf.clear();
                        compose_cursor = 0;
                    }
                }
            }

            // Toggle a soft presence claim on the active gate.
            Some(Action::ClaimGate) => {
                if let Some(wg) = &state.gate {
                    let _ = client.claim(&wg.gate_id).await;
                }
            }

            Some(Action::AbandonBegin) => {
                compose = ComposeTarget::ConfirmAbandon;
                compose_buf.clear();
                rejected_msg = Some("abandon session? [y] confirm · [esc] cancel".into());
                rejected_ttl = 60;
            }

            Some(Action::AbandonConfirm) => {
                let _ = client.abandon().await;
            }

            // Hand-edit: $EDITOR round-trip when a gate is present.
            Some(Action::HandEdit) => {
                let files: Vec<String> = if let ActiveRegion::Gate(ref card) = view.active {
                    card.file_diffs.iter().map(|fd| fd.path.clone()).collect()
                } else {
                    Vec::new()
                };
                if files.is_empty() {
                    rejected_msg = Some("no files in diff — cannot hand-edit".into());
                    rejected_ttl = 30;
                } else {
                    let path = files[selected_file % files.len()].clone();
                    // Request file contents from server.
                    let _ = client.fetch_file(&path).await;
                    // Wait for the FileContent response (10s timeout).
                    let result = tokio::time::timeout(
                        Duration::from_secs(10),
                        wait_for_file(&mut file_rx, &path),
                    )
                    .await;

                    match result {
                        Err(_elapsed) => {
                            rejected_msg = Some(format!("timed out fetching {path}"));
                            rejected_ttl = 30;
                        }
                        Ok(contents) => {
                            // Suspend TUI, launch editor, re-enter TUI.
                            TerminalGuard::restore();
                            let edit_result = run_editor_roundtrip(&path, contents.as_deref());
                            // Re-enter raw mode / alternate screen.
                            let _ = ratatui::crossterm::execute!(
                                io::stdout(),
                                ratatui::crossterm::terminal::EnterAlternateScreen,
                                ratatui::crossterm::event::EnableBracketedPaste
                            );
                            let _ = ratatui::crossterm::terminal::enable_raw_mode();

                            match edit_result {
                                Err(e) => {
                                    rejected_msg = Some(format!("editor error: {e}"));
                                    rejected_ttl = 30;
                                }
                                Ok(None) => {
                                    rejected_msg = Some("no changes".into());
                                    rejected_ttl = 20;
                                }
                                Ok(Some(new_contents)) => {
                                    let _ = client.hand_edit(&path, &new_contents).await;
                                    rejected_msg =
                                        Some("hand-edit sent — fresh gate opened".into());
                                    rejected_ttl = 40;
                                }
                            }
                        }
                    }
                }
            }

            // Scroll actions (always active in the split layout).
            Some(Action::ScrollDown) => {
                let total = diff_line_count(&view.active, selected_file);
                diff_scroll = clamp_scroll(diff_scroll as i32 + 1, total, PAGE_LINES);
            }
            Some(Action::ScrollUp) => {
                diff_scroll = clamp_scroll(diff_scroll as i32 - 1, 0, PAGE_LINES);
            }
            Some(Action::PageDown) => {
                let total = diff_line_count(&view.active, selected_file);
                diff_scroll =
                    clamp_scroll(diff_scroll as i32 + PAGE_LINES as i32, total, PAGE_LINES);
            }
            Some(Action::PageUp) => {
                diff_scroll = clamp_scroll(diff_scroll as i32 - PAGE_LINES as i32, 0, PAGE_LINES);
            }

            // Cycle selected file.
            Some(Action::CycleFile) => {
                let files_len = if let ActiveRegion::Gate(ref card) = view.active {
                    card.file_diffs.len()
                } else {
                    0
                };
                if files_len > 1 {
                    selected_file = (selected_file + 1) % files_len;
                    // Each file is its own scroll surface.
                    diff_scroll = 0;
                }
            }

            // Log scrollback: ↑ back through history, ↓ toward the tail.
            Some(Action::LogUp) => {
                log_scroll = log_scroll.saturating_add(1).min(state.log.len());
            }
            Some(Action::LogDown) => {
                log_scroll = log_scroll.saturating_sub(1);
            }

            // Host-only: approve / reject a pending BYO operator's join.
            Some(Action::ApproveJoin) => {
                if let Some(pj) = &state.pending_join {
                    let is_host = state
                        .seats
                        .first()
                        .map(|s| s.operator == own)
                        .unwrap_or(false);
                    if is_host {
                        let _ = client.resolve_join(pj.operator, true).await;
                    }
                }
            }
            Some(Action::RejectJoin) => {
                if let Some(pj) = &state.pending_join {
                    let is_host = state
                        .seats
                        .first()
                        .map(|s| s.operator == own)
                        .unwrap_or(false);
                    if is_host {
                        let _ = client.resolve_join(pj.operator, false).await;
                    }
                }
            }

            // Toggle this seat's AFK presence.
            Some(Action::ToggleAfk) => {
                let own_afk = state
                    .seats
                    .iter()
                    .find(|s| s.operator == own)
                    .map(|s| s.afk)
                    .unwrap_or(false);
                let _ = client.set_afk(!own_afk).await;
            }

            Some(Action::ToggleLink) => {
                link_mode = match link_mode {
                    LinkMode::Lan => LinkMode::Wan,
                    LinkMode::Wan => LinkMode::Lan,
                };
            }

            // Composing text.
            Some(Action::RemedyChar(c)) => {
                if matches!(compose, ComposeTarget::ConfirmAbandon) {
                    if c == 'y' {
                        let _ = client.abandon().await;
                    }
                    compose = ComposeTarget::None;
                    compose_buf.clear();
                } else {
                    compose_cursor = compose::insert_char(&mut compose_buf, compose_cursor, c);
                    // Live sync: the other seat sees the draft as it is typed.
                    if matches!(compose, ComposeTarget::Prompt) {
                        let _ = client.prompt_draft(&compose_buf).await;
                    }
                }
            }
            Some(Action::RemedyBackspace) => {
                compose_cursor = compose::backspace(&mut compose_buf, compose_cursor);
                if matches!(compose, ComposeTarget::Prompt) {
                    let _ = client.prompt_draft(&compose_buf).await;
                }
            }
            Some(Action::PasteText(text)) => {
                // Verbatim insert — newlines included; plan tasks stay
                // single-line so their list rendering can't break.
                let text = if matches!(compose, ComposeTarget::PlanEdit { .. }) {
                    text.replace('\n', " ")
                } else {
                    text
                };
                if !matches!(compose, ComposeTarget::None | ComposeTarget::ConfirmAbandon) {
                    compose_cursor = compose::insert_str(&mut compose_buf, compose_cursor, &text);
                    if matches!(compose, ComposeTarget::Prompt) {
                        let _ = client.prompt_draft(&compose_buf).await;
                    }
                }
            }
            Some(Action::NewLine) => {
                // Multi-line composes only; plan tasks are single-line.
                if matches!(
                    compose,
                    ComposeTarget::Prompt | ComposeTarget::Remedy | ComposeTarget::PlanSteer
                ) {
                    compose_cursor = compose::insert_char(&mut compose_buf, compose_cursor, '\n');
                    if matches!(compose, ComposeTarget::Prompt) {
                        let _ = client.prompt_draft(&compose_buf).await;
                    }
                }
            }
            Some(Action::CursorLeft) => {
                compose_cursor = compose::left(compose_cursor);
            }
            Some(Action::CursorRight) => {
                compose_cursor = compose::right(&compose_buf, compose_cursor);
            }
            Some(Action::CursorHome) => {
                compose_cursor = compose::home();
            }
            Some(Action::CursorEnd) => {
                compose_cursor = compose::end(&compose_buf);
            }
            Some(Action::RemedySubmit) => {
                match compose {
                    ComposeTarget::Remedy => {
                        if compose_buf.trim().is_empty() {
                            // no bare veto: keep composing until a real steer exists
                        } else {
                            if let Some(wg) = &state.gate {
                                let _ = client
                                    .cast_nogo(wg, &compose_buf, ReviewDepth::FullDiff)
                                    .await;
                            }
                            compose = ComposeTarget::None;
                            compose_buf.clear();
                        }
                    }
                    ComposeTarget::ConfirmAbandon => {
                        // Enter on confirm-abandon cancels (no bare confirm via Enter)
                        compose = ComposeTarget::None;
                        compose_buf.clear();
                    }
                    ComposeTarget::Prompt => {
                        if compose_buf.trim().is_empty() {
                            // no empty prompt: keep composing; server would also reject it
                            rejected_msg = Some("prompt cannot be empty".into());
                            rejected_ttl = 20;
                        } else {
                            let _ = client.set_prompt(&compose_buf).await;
                            compose = ComposeTarget::None;
                            compose_buf.clear();
                        }
                    }
                    ComposeTarget::PlanEdit { idx } => {
                        if compose_buf.trim().is_empty() {
                            // No blank tasks: keep composing
                            rejected_msg = Some("task cannot be empty".into());
                            rejected_ttl = 20;
                        } else {
                            if let ActiveRegion::Plan { tasks, .. } = &view.active {
                                let new_list =
                                    planedit::edit_task(tasks.clone(), idx, compose_buf.clone());
                                let _ = client.edit_plan(&new_list).await;
                            }
                            compose = ComposeTarget::None;
                            compose_buf.clear();
                        }
                    }
                    ComposeTarget::PlanSteer => {
                        if compose_buf.trim().is_empty() {
                            // No bare steer: keep composing
                            rejected_msg = Some("steer cannot be empty".into());
                            rejected_ttl = 20;
                        } else {
                            let _ = client.steer_plan(&compose_buf).await;
                            compose = ComposeTarget::None;
                            compose_buf.clear();
                        }
                    }
                    ComposeTarget::ClarifyCustom { question } => {
                        if !compose_buf.trim().is_empty() {
                            let _ = client
                                .answer(
                                    question,
                                    kontur_net::WireChoice::Custom(compose_buf.clone()),
                                )
                                .await;
                            compose = ComposeTarget::None;
                            compose_buf.clear();
                        } else {
                            rejected_msg = Some("answer cannot be empty".into());
                            rejected_ttl = 20;
                        }
                    }
                    ComposeTarget::Discuss { gate_id } => {
                        if !compose_buf.trim().is_empty() {
                            let gid = GateId(gate_id.clone());
                            let _ = client.discuss(&gid, &compose_buf).await;
                        }
                        compose = ComposeTarget::None;
                        compose_buf.clear();
                    }
                    ComposeTarget::None => {}
                }
            }
            Some(Action::RemedyCancel) => {
                // Cancelling a prompt compose must undo the streamed draft on
                // the other seat too — restore the pre-edit text.
                if matches!(compose, ComposeTarget::Prompt) {
                    let _ = client.prompt_draft(&prompt_before).await;
                }
                compose = ComposeTarget::None;
                compose_buf.clear();
            }

            Some(_) => {}
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: count diff lines for scroll clamping
// ---------------------------------------------------------------------------

/// The notice-row text for an in-progress compose, if any. Exhaustive over
/// `ComposeTarget` so every compose mode has a visible input line; `warn`
/// carries any active rejection to keep refusals visible while typing.
/// The notice-row text for the current compose, plus the char column of the
/// caret within that string (before the " > " command-row prefix) when the
/// buffer is editable in the notice. `None` column → no in-notice caret (the
/// prompt draft's caret lives in the PROMPT pane; non-buffer notices have none).
fn compose_notice(
    compose: &ComposeTarget,
    buf: &str,
    cursor: usize,
    warn: &str,
) -> Option<(String, Option<usize>)> {
    // For an "{prefix}{buf}{suffix}" notice, the caret column is the prefix
    // length plus the caret's index in the buffer (inline() is 1:1 on chars).
    let with_buf = |prefix: &str, suffix: &str| {
        let col = prefix.chars().count() + cursor.min(compose::char_len(buf));
        (
            format!("{prefix}{}{suffix}", compose::inline(buf)),
            Some(col),
        )
    };
    match compose {
        ComposeTarget::None => None,
        // The prompt draft renders in the PROMPT pane (its caret lives there).
        ComposeTarget::Prompt => Some((
            format!("editing prompt — [↵] submit · [alt+↵] newline · [esc] cancel{warn}"),
            None,
        )),
        ComposeTarget::Remedy => Some(with_buf(
            "no-go steer > ",
            &format!("  [↵] cast no-go · [alt+↵] newline · [esc] cancel{warn}"),
        )),
        ComposeTarget::PlanEdit { idx } => Some(with_buf(
            &format!("edit t{} > ", idx + 1),
            "  [↵] submit · [esc] cancel",
        )),
        ComposeTarget::PlanSteer => Some(with_buf(
            "steer > ",
            &format!("  [↵] send · [alt+↵] newline · [esc] cancel{warn}"),
        )),
        ComposeTarget::Discuss { .. } => Some(with_buf(
            "note > ",
            &format!("  [↵] post · [esc] cancel{warn}"),
        )),
        ComposeTarget::ClarifyCustom { .. } => Some(with_buf(
            "your answer > ",
            &format!("  [↵] submit · [esc] cancel{warn}"),
        )),
        ComposeTarget::ConfirmAbandon => {
            Some(("abandon session? [y] confirm · [esc] cancel".into(), None))
        }
    }
}

fn diff_line_count(active: &ActiveRegion, selected_file: usize) -> u16 {
    if let ActiveRegion::Gate(card) = active {
        if let Some(fd) = card
            .file_diffs
            .get(selected_file % card.file_diffs.len().max(1))
        {
            return fd.diff.lines().count() as u16;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Helper: wait for a FileContent message for a specific path
// ---------------------------------------------------------------------------

async fn wait_for_file(
    rx: &mut mpsc::Receiver<(String, Option<String>)>,
    wanted_path: &str,
) -> Option<String> {
    while let Some((path, contents)) = rx.recv().await {
        if path == wanted_path {
            return contents;
        }
        // Discard responses for other paths (stale requests).
    }
    None
}

// ---------------------------------------------------------------------------
// Helper: $EDITOR round-trip
// ---------------------------------------------------------------------------

/// Write `contents` to a temp file named after `path`'s basename, launch
/// $EDITOR (or "vi") blockingly, read the result back. Returns:
/// - `Ok(Some(new_contents))` if the file changed.
/// - `Ok(None)` if unchanged.
/// - `Err(e)` on I/O failure.
fn run_editor_roundtrip(path: &str, contents: Option<&str>) -> io::Result<Option<String>> {
    use std::process::Command;

    // Derive a temp file name from the basename.
    let basename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("kontur-edit");
    let tmp_path = std::env::temp_dir().join(format!("kontur-edit-{basename}"));

    // Write current contents (or empty) to temp file.
    let original = contents.unwrap_or("").to_owned();
    std::fs::write(&tmp_path, &original)?;

    // Launch the editor.
    let editor = editor_command(std::env::var("EDITOR").ok());
    let status = Command::new(&editor).arg(&tmp_path).status()?;

    if !status.success() {
        return Err(io::Error::other(format!(
            "editor exited with status {status}"
        )));
    }

    // Read back.
    let new_contents = std::fs::read_to_string(&tmp_path)?;
    // Clean up temp file (best-effort).
    let _ = std::fs::remove_file(&tmp_path);

    if new_contents == original {
        Ok(None)
    } else {
        Ok(Some(new_contents))
    }
}

// ---------------------------------------------------------------------------
// Truncation-ack pure helper (unit-tested)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GoDecision {
    /// Cast the verdict.
    Cast,
    /// Diff was truncated and the first `g` was pressed — need a second `g`.
    NeedAck,
}

/// Pure helper for the truncation-ack two-press flow.
///
/// - `truncated`: the diff preview was capped at 64 KiB.
/// - `acked`: this gate id is already in `truncation_ack` (first `g` already pressed).
pub fn go_gate(truncated: bool, acked: bool) -> GoDecision {
    if truncated && !acked {
        return GoDecision::NeedAck;
    }
    GoDecision::Cast
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kontur_core::GateId;
    use kontur_core::Hash;
    use kontur_core::{OperatorId, VerdictStatus};
    use kontur_net::{WireGate, WirePhase, WireRole, WireSeat, WireState};

    fn op(b: u8) -> OperatorId {
        OperatorId([b; 32])
    }

    fn base_state(phase: WirePhase) -> WireState {
        WireState {
            phase,
            seats: vec![
                WireSeat {
                    label: "A".into(),
                    operator: op(1),
                    role: WireRole::Host,
                    linked: true,
                    ready: false,
                    afk: false,
                },
                WireSeat {
                    label: "B".into(),
                    operator: op(2),
                    role: WireRole::Operator,
                    linked: true,
                    ready: false,
                    afk: false,
                },
            ],
            fleet: vec![],
            log: vec![],
            gate: None,
            prompt: String::new(),
            pending_join: None,
        }
    }

    fn dummy_gate(keys: Vec<kontur_core::VerdictView>) -> WireGate {
        WireGate {
            gate_id: GateId("gate-001".into()),
            task: "t1".into(),
            files: vec!["a.rs".into()],
            loc: 10,
            diff_hash: Hash([0; 32]),
            keys,
            escalation_required: false,
            file_diffs: vec![kontur_net::protocol::WireFileDiff {
                path: "a.rs".into(),
                diff: "diff --git a/a.rs b/a.rs\n+fn foo() {}".into(),
                truncated: false,
            }],
            diff_truncated: false,
            last_cmd: None,
            agent: "agent-01".into(),
            claimed_by: None,
            discuss: Vec::new(),
        }
    }

    // The instruction is surfaced after dispatch (PlanReview + Executing) and
    // hidden while composing at the dispatch gate.
    #[test]
    fn instruction_visible_after_dispatch_only() {
        let own = op(1);
        let mut planning = base_state(WirePhase::PlanReview { tasks: vec![] });
        planning.prompt = "do the thing".into();
        assert_eq!(
            wire_to_view(&planning, own, 0).instruction.as_deref(),
            Some("do the thing")
        );

        let mut executing = base_state(WirePhase::Executing);
        executing.prompt = "do the thing".into();
        assert_eq!(
            wire_to_view(&executing, own, 0).instruction.as_deref(),
            Some("do the thing")
        );

        let mut dispatch = base_state(WirePhase::DispatchReady {
            prompt: "do the thing".into(),
        });
        dispatch.prompt = "do the thing".into();
        assert!(
            wire_to_view(&dispatch, own, 0).instruction.is_none(),
            "no TASK line while composing at the dispatch gate"
        );
    }

    // Own AFK → a single calm resume line, nothing loud.
    #[test]
    fn attention_own_afk_is_calm_resume_line() {
        let own = op(1);
        let mut state = base_state(WirePhase::DispatchReady {
            prompt: "do it".into(),
        });
        state.seats[0].afk = true; // seat A is own (op(1))
        let att = attention_for(&state, own).expect("afk line");
        assert!(!att.loud, "AFK console must not be loud");
        assert!(att.text.contains("AFK"));
        assert!(att.text.contains("[z]"));
    }

    // Waiting on an AFK colleague annotates the label and stays calm.
    #[test]
    fn attention_other_afk_is_annotated() {
        let own = op(1);
        let mut state = base_state(WirePhase::DispatchReady {
            prompt: "do it".into(),
        });
        state.seats[0].ready = true; // own ready
        state.seats[1].afk = true; // other AFK, not ready
        let att = attention_for(&state, own).expect("waiting line");
        assert!(!att.loud);
        assert!(
            att.text.contains("(AFK)"),
            "must annotate the AFK colleague; got: {}",
            att.text
        );
    }

    // Sealed key stays Sealed in the view.
    #[test]
    fn sealed_key_stays_sealed() {
        let sealed_key = kontur_core::VerdictView {
            operator: op(1),
            status: VerdictStatus::Sealed,
        };
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(vec![sealed_key]));

        let view = wire_to_view(&state, op(1), 0);
        if let ActiveRegion::Gate(card) = &view.active {
            // own key is present in gate (status Sealed) — it IS in keys
            // Sealed in WireGate → Sealed in KeyView
            let own_key = card.keys.iter().find(|k| k.label == "A");
            assert!(own_key.is_some());
            assert_eq!(own_key.unwrap().status, KeyStatus::Sealed);
        } else {
            panic!("expected Gate region");
        }
    }

    // needs_you = 1 when own key is absent from gate keys.
    #[test]
    fn needs_you_when_own_key_absent() {
        // Gate has B's key but not A's.
        let b_key = kontur_core::VerdictView {
            operator: op(2),
            status: VerdictStatus::Sealed,
        };
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(vec![b_key]));

        let view = wire_to_view(&state, op(1), 0); // own = A (op(1))
        assert_eq!(view.status.needs_you, 1);
    }

    // needs_you = 0 when own key is present (even sealed).
    #[test]
    fn needs_you_zero_when_own_key_present() {
        let a_key = kontur_core::VerdictView {
            operator: op(1),
            status: VerdictStatus::Sealed,
        };
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(vec![a_key]));

        let view = wire_to_view(&state, op(1), 0); // own = A
        assert_eq!(view.status.needs_you, 0);
    }

    // DispatchReady phase → Prompt with correct ready flags.
    #[test]
    fn dispatch_ready_maps_to_prompt() {
        let mut state = base_state(WirePhase::DispatchReady {
            prompt: "do the thing".into(),
        });
        // Set seat B as ready, A not ready.
        state.seats[1].ready = true;

        let view = wire_to_view(&state, op(1), 0);
        match &view.active {
            ActiveRegion::Prompt { prompt, ready } => {
                assert_eq!(prompt, "do the thing");
                assert!(!ready[0]); // A not ready
                assert!(ready[1]); // B ready
            }
            other => panic!("expected Prompt, got {:?}", other),
        }
    }

    // Closed phase maps gates/verified/reviewers/merged.
    #[test]
    fn closed_phase_maps_correctly() {
        let state = base_state(WirePhase::Closed {
            gates: 3,
            chain_verified: true,
            reviewers: vec!["A".into(), "B".into()],
            merged: true,
            abandoned: false,
        });

        let view = wire_to_view(&state, op(1), 0);
        match &view.active {
            ActiveRegion::SessionClosed(summary) => {
                assert_eq!(summary.gates, 3);
                assert!(summary.chain_verified);
                assert_eq!(summary.reviewers, vec!["A".to_string(), "B".to_string()]);
                assert!(summary.merged);
                assert!(!summary.abandoned);
            }
            other => panic!("expected SessionClosed, got {:?}", other),
        }
    }

    // WireRole::Host maps to Role::Host (regression for casing-mismatch bug).
    #[test]
    fn wire_role_host_maps_to_host() {
        let state = base_state(WirePhase::AwaitOperators);
        let view = wire_to_view(&state, op(1), 0);
        assert_eq!(
            view.stations[0].role,
            crate::view::Role::Host,
            "seat A should be Host"
        );
        assert_eq!(
            view.stations[1].role,
            crate::view::Role::Operator,
            "seat B should be Operator"
        );
    }

    // linked=false on a seat → StatusStrip.linked == false.
    #[test]
    fn dropped_seat_sets_linked_false() {
        let mut state = base_state(WirePhase::Executing);
        state.seats[1].linked = false;

        let view = wire_to_view(&state, op(1), 0);
        assert!(!view.status.linked);
    }

    #[test]
    fn invite_gating_follows_linked_status() {
        // Mirrors the run_remote gating: invite shows only while not both linked.
        let mut state = base_state(WirePhase::Executing);
        state.seats[1].linked = false;
        let mut view = wire_to_view(&state, op(1), 0);
        let invite = Some("kontur join kontur://x:7777/aa".to_string());
        if !view.status.linked {
            view.invite = invite.clone();
        }
        assert!(view.invite.is_some());

        let state2 = base_state(WirePhase::Executing);
        let mut view2 = wire_to_view(&state2, op(1), 0);
        if !view2.status.linked {
            view2.invite = invite.clone();
        }
        assert!(view2.invite.is_none());
    }

    #[test]
    fn compose_invite_toggles_and_falls_back() {
        let both = crate::link::InviteLinks {
            lan: Some("kontur join kontur://192.168.1.2:7777/aa".into()),
            wan: Some("kontur join kontur://203.0.113.5:7777/aa".into()),
            port: 7777,
        };
        let lan = compose_invite_text(&both, LinkMode::Lan).unwrap();
        assert!(lan.contains("192.168.1.2"));
        assert!(lan.contains("[l] switch to WAN"));
        assert!(!lan.contains("forward port"));

        let wan = compose_invite_text(&both, LinkMode::Wan).unwrap();
        assert!(wan.contains("203.0.113.5"));
        assert!(wan.contains("forward port 7777"));
        assert!(wan.contains("[l] switch to LAN"));

        let lan_only = crate::link::InviteLinks {
            lan: both.lan.clone(),
            wan: None,
            port: 7777,
        };
        let t = compose_invite_text(&lan_only, LinkMode::Wan).unwrap();
        assert!(t.contains("192.168.1.2")); // falls back
        assert!(!t.contains("[l] switch")); // no toggle hint with one flavour
        assert!(!t.contains("forward port")); // fallback is LAN, no WAN caveat

        assert!(compose_invite_text(
            &crate::link::InviteLinks {
                lan: None,
                wan: None,
                port: 7777
            },
            LinkMode::Lan
        )
        .is_none());
    }

    // -----------------------------------------------------------------------
    // go_gate pure helper tests (truncation ack)
    // -----------------------------------------------------------------------

    #[test]
    fn go_gate_need_ack_on_first_g_with_truncated_diff() {
        use super::GoDecision;
        assert_eq!(super::go_gate(true, false), GoDecision::NeedAck);
    }

    #[test]
    fn go_gate_cast_when_acked_or_not_truncated() {
        use super::GoDecision;
        assert_eq!(super::go_gate(false, false), GoDecision::Cast);
        assert_eq!(super::go_gate(true, true), GoDecision::Cast);
        assert_eq!(super::go_gate(false, true), GoDecision::Cast);
    }

    // FR-24: a verdict built with FullDiff depth carries that depth.
    #[test]
    fn cast_verdict_carries_full_diff_depth() {
        use kontur_core::{
            CastVerdict, Ed25519Signer, FixedClock, GateId, Hash, Remedy, ReviewDepth, Verdict,
        };
        let signer = Ed25519Signer::from_seed([5u8; 32]);
        let gate_id = GateId("gate-fr24".into());
        let diff_hash = Hash([0u8; 32]);
        let cv = CastVerdict::create(
            &signer,
            &FixedClock(42),
            &gate_id,
            diff_hash,
            Verdict::NoGo(Remedy::Steer("needs tests".into())),
            ReviewDepth::FullDiff,
            None,
        );
        assert_eq!(
            cv.depth,
            ReviewDepth::FullDiff,
            "CastVerdict must carry FullDiff depth"
        );
    }

    // -----------------------------------------------------------------------
    // attention_for pure helper tests
    // -----------------------------------------------------------------------

    // DispatchReady, own not ready → loud "confirm the prompt"
    #[test]
    fn attention_dispatch_own_not_ready_is_loud() {
        let state = base_state(WirePhase::DispatchReady {
            prompt: "do the thing".into(),
        });
        // seats[0] = op(1), not ready (default)
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(att.loud, "must be loud when own not ready at dispatch");
        assert!(att.text.contains("confirm the prompt"));
    }

    // DispatchReady, own ready, other not → calm "waiting on B"
    #[test]
    fn attention_dispatch_own_ready_other_not_is_calm() {
        let mut state = base_state(WirePhase::DispatchReady {
            prompt: "do the thing".into(),
        });
        state.seats[0].ready = true; // op(1) (A) is ready
                                     // op(2) (B) stays not ready
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(!att.loud, "must be calm when waiting on other");
        assert!(att.text.contains("B"), "must name the other seat");
    }

    // DispatchReady, both ready → None
    #[test]
    fn attention_dispatch_both_ready_is_none() {
        let mut state = base_state(WirePhase::DispatchReady {
            prompt: "do the thing".into(),
        });
        state.seats[0].ready = true;
        state.seats[1].ready = true;
        assert!(
            super::attention_for(&state, op(1)).is_none(),
            "both ready → no attention line"
        );
    }

    // PlanReview, tasks empty → calm "waiting on agent's plan"
    #[test]
    fn attention_plan_review_no_tasks_is_calm() {
        let state = base_state(WirePhase::PlanReview { tasks: vec![] });
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(!att.loud, "waiting for plan → calm");
        assert!(att.text.contains("agent's plan"));
    }

    // PlanReview, tasks present, own not ready → loud "review the plan"
    #[test]
    fn attention_plan_review_own_not_ready_is_loud() {
        let state = base_state(WirePhase::PlanReview {
            tasks: vec!["t1".into()],
        });
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(att.loud, "must be loud when own not ready at plan review");
        assert!(att.text.contains("review the plan"));
    }

    // PlanReview, own ready, other not → calm "waiting on B"
    #[test]
    fn attention_plan_review_own_ready_other_not_is_calm() {
        let mut state = base_state(WirePhase::PlanReview {
            tasks: vec!["t1".into()],
        });
        state.seats[0].ready = true; // A ready
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(!att.loud, "waiting on other → calm");
        assert!(att.text.contains("B"), "must name other seat");
    }

    // Executing, gate present, own key absent → loud "review the diff and cast"
    #[test]
    fn attention_executing_gate_own_absent_is_loud() {
        // Gate has only B's key
        let b_key = kontur_core::VerdictView {
            operator: op(2),
            status: VerdictStatus::Sealed,
        };
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(vec![b_key]));
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(att.loud, "must be loud when own key absent from gate");
        assert!(att.text.contains("review the diff"));
    }

    // Executing, gate present, own key present, other absent → calm sealed waiting
    #[test]
    fn attention_executing_gate_own_sealed_other_absent_is_calm() {
        let a_key = kontur_core::VerdictView {
            operator: op(1),
            status: VerdictStatus::Sealed,
        };
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(vec![a_key]));
        let att = super::attention_for(&state, op(1)).expect("should have attention");
        assert!(!att.loud, "own key sealed, waiting on other → calm");
        assert!(att.text.contains("sealed"), "must mention sealed");
        assert!(att.text.contains("B"), "must name other seat");
    }

    // Executing, gate present, both keys present → None
    #[test]
    fn attention_executing_gate_both_keys_present_is_none() {
        let keys = vec![
            kontur_core::VerdictView {
                operator: op(1),
                status: VerdictStatus::Sealed,
            },
            kontur_core::VerdictView {
                operator: op(2),
                status: VerdictStatus::Sealed,
            },
        ];
        let mut state = base_state(WirePhase::Executing);
        state.gate = Some(dummy_gate(keys));
        assert!(
            super::attention_for(&state, op(1)).is_none(),
            "both keys present → no attention line"
        );
    }

    // Executing, no gate → None
    #[test]
    fn attention_executing_no_gate_is_none() {
        let state = base_state(WirePhase::Executing);
        assert!(
            super::attention_for(&state, op(1)).is_none(),
            "executing without gate → None"
        );
    }

    // AwaitOperators → None
    #[test]
    fn attention_await_operators_is_none() {
        let state = base_state(WirePhase::AwaitOperators);
        assert!(super::attention_for(&state, op(1)).is_none());
    }

    // Closed → None
    #[test]
    fn attention_closed_is_none() {
        let state = base_state(WirePhase::Closed {
            gates: 1,
            chain_verified: true,
            reviewers: vec![],
            merged: true,
            abandoned: false,
        });
        assert!(super::attention_for(&state, op(1)).is_none());
    }

    // ---- compose_notice: every compose mode has a visible input line ----

    #[test]
    fn every_compose_mode_renders_a_notice() {
        let modes: Vec<ComposeTarget> = vec![
            ComposeTarget::Prompt,
            ComposeTarget::Remedy,
            ComposeTarget::PlanEdit { idx: 0 },
            ComposeTarget::PlanSteer,
            ComposeTarget::ConfirmAbandon,
        ];
        for mode in &modes {
            assert!(
                compose_notice(mode, "draft", 5, "").is_some(),
                "compose mode without a visible input line"
            );
        }
        assert!(compose_notice(&ComposeTarget::None, "", 0, "").is_none());
    }

    #[test]
    fn remedy_compose_shows_buffer_and_warning() {
        let (n, caret) = compose_notice(
            &ComposeTarget::Remedy,
            "add a test",
            3,
            " · steer cannot be empty",
        )
        .unwrap();
        assert!(n.contains("no-go steer > add a test"));
        assert!(n.contains("steer cannot be empty"));
        assert!(n.contains("[esc] cancel"));
        // caret column = "no-go steer > " (14) + cursor (3).
        assert_eq!(caret, Some(17));
    }
}
