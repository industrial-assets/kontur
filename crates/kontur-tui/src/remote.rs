//! Remote two-seat mode: connects to a kontur-net SessionServer over TCP,
//! maps WireState → SessionView, and runs the interactive terminal loop.

use std::io;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use kontur_core::{OperatorId, VerdictStatus, Verdict};
use kontur_net::{ServerMsg, SessionClient, WireGate, WirePhase, WireState};

use crate::app::{poll_action, TerminalGuard};
use crate::input::Action;
use crate::render::{render, render_diff};
use crate::view::{
    ActiveRegion, AgentCard, AuditSummary, Banner, GateCard, KeyStatus, KeyView, LogLine, Role,
    SessionView, Station, StatusStrip,
};

// ---------------------------------------------------------------------------
// Compose-state machine (local to the loop)
// ---------------------------------------------------------------------------

enum ComposeTarget {
    None,
    Remedy,
    HandEditPath,
    HandEditContents { path: String },
}

// ---------------------------------------------------------------------------
// wire_to_view
// ---------------------------------------------------------------------------

/// Map a WireState snapshot to a pure SessionView. The `own` id is used to
/// compute `needs_you` and is not exposed in the rendered output.
pub fn wire_to_view(state: &WireState, own: OperatorId) -> SessionView {
    // --- stations ---
    let stations: [Station; 2] = {
        let mut iter = state.seats.iter();
        let make = |ws: &kontur_net::WireSeat| Station {
            label: ws.label.clone(),
            role: if ws.role == "DRIVER" { Role::Driver } else { Role::Navigator },
            activity: if ws.linked { "linked".into() } else { "dropped".into() },
            operator: ws.operator,
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
                    role: Role::Navigator,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                },
            ],
            _ => [
                Station {
                    label: "A".into(),
                    role: Role::Driver,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                },
                Station {
                    label: "B".into(),
                    role: Role::Navigator,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
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
            tokens: f.tokens,
            needs_signoff: f.needs_signoff,
        })
        .collect();

    // --- log ---
    let log: Vec<LogLine> = state
        .log
        .iter()
        .map(|l| LogLine { time: String::new(), who: String::new(), text: l.clone() })
        .collect();

    // --- status strip ---
    let both_linked = state.seats.iter().all(|s| s.linked);
    let fleet_count = fleet.len();
    let tokens: u64 = fleet.iter().map(|a| a.tokens).sum();

    // needs_you: count pending gates (gate present + own key not yet in keys)
    let needs_you = if let Some(gate) = &state.gate {
        let own_has_key = gate.keys.iter().any(|k| k.operator == own);
        if own_has_key { 0 } else { 1 }
    } else {
        0
    };

    let status = StatusStrip {
        linked: both_linked,
        four_eyes: true,
        fleet_count,
        needs_you,
        tokens,
    };

    // --- active region ---
    let active = match &state.phase {
        WirePhase::AwaitOperators => ActiveRegion::Idle,
        WirePhase::DispatchReady { prompt } => {
            let ready = [
                state.seats.first().map(|s| s.ready).unwrap_or(false),
                state.seats.get(1).map(|s| s.ready).unwrap_or(false),
            ];
            ActiveRegion::Prompt { prompt: prompt.clone(), ready }
        }
        WirePhase::PlanReview { tasks } => {
            let ready = [
                state.seats.first().map(|s| s.ready).unwrap_or(false),
                state.seats.get(1).map(|s| s.ready).unwrap_or(false),
            ];
            ActiveRegion::Plan { tasks: tasks.clone(), ready }
        }
        WirePhase::Executing => {
            if let Some(wg) = &state.gate {
                ActiveRegion::Gate(wire_gate_to_card(wg, &stations))
            } else {
                ActiveRegion::Idle
            }
        }
        WirePhase::Closed { gates, chain_verified, reviewers } => {
            ActiveRegion::SessionClosed(AuditSummary {
                gates: *gates,
                chain_verified: *chain_verified,
                reviewers: reviewers.clone(),
            })
        }
    };

    SessionView {
        banner: Banner { session: "remote".into(), version: env!("CARGO_PKG_VERSION").into() },
        status,
        stations,
        fleet,
        log,
        active,
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
            KeyView { label: st.label.clone(), role: st.role, status }
        })
        .collect();

    GateCard {
        gate_id: wg.gate_id.0.clone(),
        task: wg.task.clone(),
        files: wg.files.clone(),
        loc: wg.loc,
        keys,
        escalation_required: wg.escalation_required,
        diff_preview: wg.diff_preview.clone(),
    }
}

// ---------------------------------------------------------------------------
// run_remote
// ---------------------------------------------------------------------------

/// Connect to a kontur-net session server, enter the TUI, and loop until quit.
pub async fn run_remote(addr: &str, seat: String, seed: [u8; 32]) -> io::Result<()> {
    let (client, mut rx) = SessionClient::connect_tcp(addr, seat, seed).await?;
    let own = client.operator();

    // Fold the mpsc stream into a watch so the render loop always has the
    // latest state without blocking.
    let initial = WireState {
        phase: WirePhase::AwaitOperators,
        seats: vec![],
        fleet: vec![],
        log: vec![],
        gate: None,
    };
    let (state_tx, state_rx) = watch::channel(initial);

    // Track transient rejection reason.
    let (rej_tx, mut rej_rx) = mpsc::channel::<String>(4);

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
            }
        }
    });

    let (_guard, mut terminal) = TerminalGuard::enter()?;

    let mut compose = ComposeTarget::None;
    let mut compose_buf = String::new();
    let mut diff_open = false;
    let mut rejected_msg: Option<String> = None;
    let mut rejected_ttl: u8 = 0;

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

        let state = state_rx.borrow().clone();
        let view = wire_to_view(&state, own);

        terminal.draw(|f| {
            if diff_open {
                if let ActiveRegion::Gate(ref card) = view.active {
                    if let Some(preview) = &card.diff_preview {
                        render_diff(f, &card.gate_id, preview);
                        return;
                    }
                }
            }
            render(f, &view);
        })?;

        let composing = !matches!(compose, ComposeTarget::None);
        match poll_action(Duration::from_millis(200), composing)? {
            None => {}
            Some(Action::Quit) => break,

            // Ready signal (dispatch / plan approval).
            Some(Action::Ready) => {
                let _ = client.ready().await;
            }

            // Go verdict.
            Some(Action::Go) => {
                if let Some(wg) = &state.gate {
                    let _ = client.cast_go(wg).await;
                }
            }

            // No-go → start remedy compose.
            Some(Action::NoGoBegin) => {
                compose = ComposeTarget::Remedy;
                compose_buf.clear();
            }

            // Hand-edit → start path compose.
            Some(Action::HandEdit) => {
                compose = ComposeTarget::HandEditPath;
                compose_buf.clear();
            }

            // Diff toggle.
            Some(Action::OpenDiff) => {
                diff_open = !diff_open;
            }

            // Role rotation.
            Some(Action::RotateRole) => {
                let _ = client.rotate().await;
            }

            // Composing text.
            Some(Action::RemedyChar(c)) => {
                compose_buf.push(c);
            }
            Some(Action::RemedyBackspace) => {
                compose_buf.pop();
            }
            Some(Action::RemedySubmit) => {
                match compose {
                    ComposeTarget::Remedy => {
                        if let Some(wg) = &state.gate {
                            let _ = client.cast_nogo(wg, &compose_buf).await;
                        }
                        compose = ComposeTarget::None;
                        compose_buf.clear();
                    }
                    ComposeTarget::HandEditPath => {
                        let path = compose_buf.clone();
                        compose = ComposeTarget::HandEditContents { path };
                        compose_buf.clear();
                    }
                    ComposeTarget::HandEditContents { ref path } => {
                        let path = path.clone();
                        let _ = client.hand_edit(&path, &compose_buf).await;
                        compose = ComposeTarget::None;
                        compose_buf.clear();
                    }
                    ComposeTarget::None => {}
                }
            }
            Some(Action::RemedyCancel) => {
                compose = ComposeTarget::None;
                compose_buf.clear();
            }

            Some(_) => {}
        }

        // Show rejection reason on stderr (transient).
        if let Some(ref msg) = rejected_msg {
            eprintln!("REJECTED: {}", msg);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kontur_core::{VerdictStatus, OperatorId};
    use kontur_net::{WireGate, WirePhase, WireSeat, WireState};
    use kontur_core::GateId;
    use kontur_core::Hash;

    fn op(b: u8) -> OperatorId {
        OperatorId([b; 32])
    }

    fn base_state(phase: WirePhase) -> WireState {
        WireState {
            phase,
            seats: vec![
                WireSeat { label: "A".into(), operator: op(1), role: "DRIVER".into(), linked: true, ready: false },
                WireSeat { label: "B".into(), operator: op(2), role: "NAVIGATOR".into(), linked: true, ready: false },
            ],
            fleet: vec![],
            log: vec![],
            gate: None,
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
            diff_preview: Some("diff --git a/a.rs b/a.rs\n+fn foo() {}".into()),
        }
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

        let view = wire_to_view(&state, op(1));
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

        let view = wire_to_view(&state, op(1)); // own = A (op(1))
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

        let view = wire_to_view(&state, op(1)); // own = A
        assert_eq!(view.status.needs_you, 0);
    }

    // DispatchReady phase → Prompt with correct ready flags.
    #[test]
    fn dispatch_ready_maps_to_prompt() {
        let mut state = base_state(WirePhase::DispatchReady { prompt: "do the thing".into() });
        // Set seat B as ready, A not ready.
        state.seats[1].ready = true;

        let view = wire_to_view(&state, op(1));
        match &view.active {
            ActiveRegion::Prompt { prompt, ready } => {
                assert_eq!(prompt, "do the thing");
                assert!(!ready[0]); // A not ready
                assert!(ready[1]);  // B ready
            }
            other => panic!("expected Prompt, got {:?}", other),
        }
    }

    // Closed phase maps gates/verified/reviewers.
    #[test]
    fn closed_phase_maps_correctly() {
        let state = base_state(WirePhase::Closed {
            gates: 3,
            chain_verified: true,
            reviewers: vec!["A".into(), "B".into()],
        });

        let view = wire_to_view(&state, op(1));
        match &view.active {
            ActiveRegion::SessionClosed(summary) => {
                assert_eq!(summary.gates, 3);
                assert!(summary.chain_verified);
                assert_eq!(summary.reviewers, vec!["A".to_string(), "B".to_string()]);
            }
            other => panic!("expected SessionClosed, got {:?}", other),
        }
    }

    // linked=false on a seat → StatusStrip.linked == false.
    #[test]
    fn dropped_seat_sets_linked_false() {
        let mut state = base_state(WirePhase::Executing);
        state.seats[1].linked = false;

        let view = wire_to_view(&state, op(1));
        assert!(!view.status.linked);
    }
}
