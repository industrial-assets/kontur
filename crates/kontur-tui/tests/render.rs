use kontur_core::OperatorId;
use kontur_tui::render::render;
use kontur_tui::view::{
    ActiveRegion, Attention, AuditSummary, Banner, GateCard, KeyStatus, KeyView, Role, SessionView,
    Station, StatusStrip,
};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

fn buf_string(buf: &Buffer) -> String {
    let area = buf.area;
    let mut s = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

fn base(active: ActiveRegion) -> SessionView {
    SessionView {
        banner: Banner {
            session: "4417".into(),
            version: "0.1.0".into(),
        },
        status: StatusStrip {
            linked: true,
            four_eyes: true,
            fleet_count: 3,
            needs_you: 1,
            tokens: 6400,
        },
        stations: [
            Station {
                label: "A · YOU".into(),
                role: Role::Host,
                activity: "watching".into(),
                operator: OperatorId([1; 32]),
            },
            Station {
                label: "B · J.REED".into(),
                role: Role::Operator,
                activity: "reviewing".into(),
                operator: OperatorId([2; 32]),
            },
        ],
        fleet: vec![],
        log: vec![],
        active,
        invite: None,
        notice: None,
        attention: None,
    }
}

fn draw(view: &SessionView) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|f| render(f, view, 0, 0)).unwrap();
    buf_string(terminal.backend().buffer())
}

#[test]
fn banner_and_status_render() {
    let s = draw(&base(ActiveRegion::Idle));
    assert!(s.contains("КОНТУР-1"));
    assert!(s.contains("4-EYES ON"));
    assert!(s.contains("NEEDS YOU"));
}

#[test]
fn gate_shows_summary_and_sealed_key_never_value() {
    let card = GateCard {
        gate_id: "gate-001".into(),
        task: "t1".into(),
        files: vec!["auth/session.rs".into()],
        loc: 47,
        keys: vec![
            KeyView {
                label: "A · YOU".into(),
                role: Role::Host,
                status: KeyStatus::Awaiting,
            },
            KeyView {
                label: "B · J.REED".into(),
                role: Role::Operator,
                status: KeyStatus::Sealed,
            },
        ],
        escalation_required: false,
        diff_preview: None,
        diff_truncated: false,
    };
    let s = draw(&base(ActiveRegion::Gate(card)));
    assert!(s.contains("auth/session.rs"));
    assert!(s.contains("cast — sealed"));
    // The sealed key must not reveal a verdict value.
    assert!(!s.contains("■ GO"));
    assert!(!s.contains("■ NO-GO"));
    assert!(s.contains("[g] go"));
}

#[test]
fn session_close_shows_verified_chain() {
    let summary = AuditSummary {
        gates: 4,
        reviewers: vec!["A · YOU".into(), "B · J.REED".into()],
        chain_verified: true,
        merged: true,
        abandoned: false,
    };
    let s = draw(&base(ActiveRegion::SessionClosed(summary)));
    assert!(s.contains("4 gates"));
    assert!(!s.contains("unanimous"));
    assert!(s.contains("chain verified"));
    assert!(s.contains("Reviewed-by: A · YOU"));
    assert!(s.contains("merged to repo"));
}

#[test]
fn prompt_region_renders_dispatch_gate_and_ready_key() {
    let s = draw(&base(ActiveRegion::Prompt {
        prompt: "refactor session guard".into(),
        ready: [false, true],
    }));
    assert!(s.contains("DISPATCH GATE"));
    assert!(s.contains("[y] mark ready"));
    assert!(s.contains("refactor session guard"));
}

#[test]
fn plan_region_renders_plan_gate_and_approve_key() {
    let s = draw(&base(ActiveRegion::Plan {
        tasks: vec!["auth.rs".into(), "session.rs".into()],
        ready: [true, false],
        selected: 0,
    }));
    assert!(s.contains("PLAN GATE"));
    assert!(s.contains("[y] approve"));
    assert!(s.contains("[r] steer replan"));
}

#[test]
fn dropped_link_shows_b_station_dropped() {
    let mut view = base(ActiveRegion::Idle);
    view.status.linked = false;
    let s = draw(&view);
    assert!(s.contains("B-STATION DROPPED"));
}

#[test]
fn session_close_no_longer_says_unanimous() {
    let summary = AuditSummary {
        gates: 4,
        reviewers: vec!["A".into()],
        chain_verified: true,
        merged: true,
        abandoned: false,
    };
    let s = draw(&base(ActiveRegion::SessionClosed(summary)));
    assert!(s.contains("4 gates"));
    assert!(!s.contains("unanimous"));
}

#[test]
fn invite_panel_shows_full_link_when_set() {
    let mut view = base(ActiveRegion::Idle);
    view.status.linked = false;
    view.invite = Some("kontur join kontur://203.0.113.5:7777/aabbccdd".into());
    let s = draw(&view);
    assert!(s.contains("INVITE — OPERATOR NOT LINKED"));
    assert!(s.contains("kontur join kontur://203.0.113.5:7777/aabbccdd"));
    assert!(s.contains("the link IS the operator's key"));
}

#[test]
fn invite_panel_absent_when_none() {
    let s = draw(&base(ActiveRegion::Idle));
    assert!(!s.contains("INVITE"));
}

#[test]
fn invite_panel_renders_remote_variant_line() {
    let mut view = base(ActiveRegion::Idle);
    view.status.linked = false;
    view.invite = Some(
        "kontur join kontur://192.168.1.10:7777/aabb\nremote (forward port 7777 first): kontur join kontur://203.0.113.5:7777/aabb"
            .into(),
    );
    let s = draw(&view);
    assert!(s.contains("kontur://192.168.1.10:7777/aabb"));
    assert!(s.contains("remote (forward port 7777 first)"));
}

/// Gate: diff always visible in right pane alongside LOG in left pane.
#[test]
fn gate_shows_diff_and_log_simultaneously() {
    let card = GateCard {
        gate_id: "gate-001".into(),
        task: "t1".into(),
        files: vec!["auth/session.rs".into()],
        loc: 47,
        keys: vec![
            KeyView {
                label: "A · YOU".into(),
                role: Role::Host,
                status: KeyStatus::Awaiting,
            },
            KeyView {
                label: "B · J.REED".into(),
                role: Role::Operator,
                status: KeyStatus::Sealed,
            },
        ],
        escalation_required: false,
        diff_preview: Some("diff --git a/auth/session.rs b/auth/session.rs\n+fn foo() {}".into()),
        diff_truncated: false,
    };
    let s = draw(&base(ActiveRegion::Gate(card)));
    // Both left-pane LOG and right-pane DIFF must be visible at once.
    assert!(s.contains("LOG"), "LOG title must appear in left pane");
    assert!(s.contains("DIFF"), "DIFF title must appear in right pane");
    // Verdict bar must also be visible.
    assert!(s.contains("[g] go"), "verdict bar must show [g] go");
}

/// Gate: truncated diff shows (TRUNCATED) in title.
#[test]
fn gate_truncated_flag_shows_truncated_in_diff_title() {
    let card = GateCard {
        gate_id: "gate-trunc".into(),
        task: "t1".into(),
        files: vec!["big.rs".into()],
        loc: 9999,
        keys: vec![],
        escalation_required: false,
        diff_preview: Some("diff --git a/big.rs b/big.rs\n+fn big() {}".into()),
        diff_truncated: true,
    };
    let s = draw(&base(ActiveRegion::Gate(card)));
    assert!(
        s.contains("TRUNCATED"),
        "truncated diff must show TRUNCATED in title; got:\n{s}"
    );
}

/// Attention: loud text (BOLD+REVERSED) renders below the status strip.
#[test]
fn attention_loud_renders_text() {
    let mut view = base(ActiveRegion::Idle);
    view.attention = Some(Attention {
        text: "▶ ACTION: confirm the prompt — [y] ready · [p] edit".into(),
        loud: true,
    });
    let s = draw(&view);
    assert!(
        s.contains("▶ ACTION: confirm the prompt"),
        "loud attention text must appear in rendered output; got:\n{s}"
    );
}

/// Attention: calm text (DIM) renders below the status strip.
#[test]
fn attention_calm_renders_text() {
    let mut view = base(ActiveRegion::Idle);
    view.attention = Some(Attention {
        text: "waiting on B · J.REED to confirm".into(),
        loud: false,
    });
    let s = draw(&view);
    assert!(
        s.contains("waiting on B"),
        "calm attention text must appear in rendered output; got:\n{s}"
    );
}

/// Attention: None → no attention text appears.
#[test]
fn attention_none_renders_nothing() {
    let s = draw(&base(ActiveRegion::Idle));
    // No attention text should leak into the output (attention is None in base()).
    assert!(
        !s.contains("▶ ACTION"),
        "no attention text when attention is None; got:\n{s}"
    );
}

/// Gate: files bar shows ▶ for the selected file.
#[test]
fn gate_files_bar_shows_selection_marker() {
    let card = GateCard {
        gate_id: "gate-002".into(),
        task: "t2".into(),
        files: vec!["a.rs".into(), "b.rs".into()],
        loc: 10,
        keys: vec![],
        escalation_required: false,
        diff_preview: None,
        diff_truncated: false,
    };
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|f| render(f, &base(ActiveRegion::Gate(card)), 0, 1))
        .unwrap();
    let s = buf_string(terminal.backend().buffer());
    assert!(
        s.contains("▶ b.rs"),
        "selected file must be marked with ▶; got:\n{s}"
    );
}
