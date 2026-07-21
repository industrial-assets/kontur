//! Remote two-seat mode: connects to a kontur-net SessionServer over TCP,
//! maps WireState → SessionView, and runs the interactive terminal loop.

use std::io;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use kontur_core::{OperatorId, ReviewDepth, Verdict, VerdictStatus};
use kontur_net::{ServerMsg, SessionClient, WireGate, WirePhase, WireRole, WireState};

use crate::app::{poll_action, TerminalGuard};
use crate::diffview::{clamp_scroll, diff_files, editor_command};
use crate::input::Action;
use crate::planedit;
use crate::render::render;
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
    ConfirmAbandon,
    Prompt,
    /// Editing a plan task in-place. `idx` is the task's index in the list.
    PlanEdit {
        idx: usize,
    },
    /// Composing a plan steer prompt.
    PlanSteer,
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
                },
            ],
            _ => [
                Station {
                    label: "A".into(),
                    role: Role::Host,
                    activity: "absent".into(),
                    operator: OperatorId([0; 32]),
                },
                Station {
                    label: "B".into(),
                    role: Role::Operator,
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
        .map(|l| LogLine {
            time: String::new(),
            who: String::new(),
            text: l.clone(),
        })
        .collect();

    // --- status strip ---
    let both_linked = state.seats.iter().all(|s| s.linked);
    let fleet_count = fleet.len();
    let tokens: u64 = fleet.iter().map(|a| a.tokens).sum();

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
        diff_preview: wg.diff_preview.clone(),
        diff_truncated: wg.diff_truncated,
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
    };
    let (state_tx, state_rx) = watch::channel(initial);

    // Track transient rejection reason.
    let (rej_tx, mut rej_rx) = mpsc::channel::<String>(4);

    // Dedicated channel for FileContent responses.
    let (file_tx, mut file_rx) = mpsc::channel::<(String, Option<String>)>(4);

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
            }
        }
    });

    let (_guard, mut terminal) = TerminalGuard::enter()?;

    let mut compose = ComposeTarget::None;
    let mut compose_buf = String::new();
    let mut diff_scroll: u16 = 0;
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

        let state = state_rx.borrow().clone();
        // Clamp plan_sel whenever the task list changes (remote edits can shrink it).
        if let WirePhase::PlanReview { tasks } = &state.phase {
            plan_sel = planedit::clamp_sel(plan_sel, tasks.len());
        }
        let mut view = wire_to_view(&state, own, plan_sel);
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
        // ConfirmAbandon state: surface the confirm prompt via notice while
        // composing (ttl may not be set yet if the state was just entered).
        if matches!(compose, ComposeTarget::ConfirmAbandon) && view.notice.is_none() {
            view.notice = Some("abandon session? [y] confirm · [esc] cancel".into());
        }
        // Prompt compose: show draft in the notice row (consistent with remedy
        // compose). Keep any active rejection visible alongside the draft —
        // otherwise "prompt cannot be empty" would be clobbered on the next
        // frame and the operator would get silent refusals.
        if matches!(compose, ComposeTarget::Prompt) {
            let warn = if rejected_ttl > 0 {
                rejected_msg
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            view.notice = Some(format!(
                "prompt > {compose_buf}  [↵] submit · [esc] cancel{warn}"
            ));
        }
        // Plan task edit compose: show the edit buffer in the notice row.
        if let ComposeTarget::PlanEdit { idx } = &compose {
            view.notice = Some(format!(
                "edit t{} > {compose_buf}  [↵] submit · [esc] cancel",
                idx + 1
            ));
        }
        // Plan steer compose: show the steer buffer in the notice row.
        if matches!(compose, ComposeTarget::PlanSteer) {
            let warn = if rejected_ttl > 0 {
                rejected_msg
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            view.notice = Some(format!(
                "steer > {compose_buf}  [↵] send · [esc] cancel{warn}"
            ));
        }

        // When a gate is pending with multiple files, show file-cycle hint in notice.
        if view.notice.is_none() {
            if let ActiveRegion::Gate(ref card) = view.active {
                if let Some(ref preview) = card.diff_preview {
                    let files = diff_files(preview);
                    if files.len() > 1 {
                        let path = files.get(selected_file).map(String::as_str).unwrap_or("");
                        view.notice = Some(format!("[tab] file: {path}"));
                    }
                }
            }
        }

        terminal.draw(|f| {
            render(f, &view, diff_scroll, selected_file);
        })?;

        let composing = !matches!(compose, ComposeTarget::None);
        let in_plan_review = matches!(view.active, ActiveRegion::Plan { .. }) && !composing;
        match poll_action(Duration::from_millis(200), composing, in_plan_review)? {
            None => {}
            Some(Action::Quit) => break,

            // Ready signal (dispatch / plan approval).
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
                                "diff was truncated at 64 KB — press [g] again to sign anyway"
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
            }

            // Prompt edit → start composing (valid only in DispatchReady region).
            Some(Action::PromptBegin) => {
                if let ActiveRegion::Prompt { prompt, .. } = &view.active {
                    compose = ComposeTarget::Prompt;
                    // Seed with the current prompt so small edits don't require
                    // retyping the whole instruction (same idiom as task editing).
                    // Draft is shown via the notice row while composing.
                    compose_buf = prompt.clone();
                }
            }

            // Abandon → request confirmation.
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
                let diff_text = if let ActiveRegion::Gate(ref card) = view.active {
                    card.diff_preview.clone()
                } else {
                    None
                };
                let files = diff_text.as_deref().map(diff_files).unwrap_or_default();
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
                                ratatui::crossterm::terminal::EnterAlternateScreen
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
                let total = diff_line_count(&view.active);
                diff_scroll = clamp_scroll(diff_scroll as i32 + 1, total, PAGE_LINES);
            }
            Some(Action::ScrollUp) => {
                diff_scroll = clamp_scroll(diff_scroll as i32 - 1, 0, PAGE_LINES);
            }
            Some(Action::PageDown) => {
                let total = diff_line_count(&view.active);
                diff_scroll =
                    clamp_scroll(diff_scroll as i32 + PAGE_LINES as i32, total, PAGE_LINES);
            }
            Some(Action::PageUp) => {
                diff_scroll = clamp_scroll(diff_scroll as i32 - PAGE_LINES as i32, 0, PAGE_LINES);
            }

            // Cycle selected file.
            Some(Action::CycleFile) => {
                let files_len = if let ActiveRegion::Gate(ref card) = view.active {
                    card.diff_preview
                        .as_deref()
                        .map(diff_files)
                        .unwrap_or_default()
                        .len()
                } else {
                    0
                };
                if files_len > 1 {
                    selected_file = (selected_file + 1) % files_len;
                }
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
                    compose_buf.push(c);
                }
            }
            Some(Action::RemedyBackspace) => {
                compose_buf.pop();
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
                    ComposeTarget::None => {}
                }
            }
            Some(Action::RemedyCancel) => {
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

fn diff_line_count(active: &ActiveRegion) -> u16 {
    if let ActiveRegion::Gate(card) = active {
        if let Some(ref preview) = card.diff_preview {
            return preview.lines().count() as u16;
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
                },
                WireSeat {
                    label: "B".into(),
                    operator: op(2),
                    role: WireRole::Operator,
                    linked: true,
                    ready: false,
                },
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
            diff_truncated: false,
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
}
