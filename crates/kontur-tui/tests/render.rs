use kontur_core::OperatorId;
use kontur_tui::render::render;
use kontur_tui::view::{
    ActiveRegion, AuditSummary, Banner, GateCard, KeyStatus, KeyView, Role, SessionView, Station,
    StatusStrip,
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
        banner: Banner { session: "4417".into(), version: "0.1.0".into() },
        status: StatusStrip { linked: true, four_eyes: true, fleet_count: 3, needs_you: 1, tokens: 6400 },
        stations: [
            Station { label: "A · YOU".into(), role: Role::Host, activity: "watching".into(), operator: OperatorId([1; 32]) },
            Station { label: "B · J.REED".into(), role: Role::Operator, activity: "reviewing".into(), operator: OperatorId([2; 32]) },
        ],
        fleet: vec![],
        log: vec![],
        active,
        invite: None,
    }
}

fn draw(view: &SessionView) -> String {
    let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
    terminal.draw(|f| render(f, view)).unwrap();
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
            KeyView { label: "A · YOU".into(), role: Role::Host, status: KeyStatus::Awaiting },
            KeyView { label: "B · J.REED".into(), role: Role::Operator, status: KeyStatus::Sealed },
        ],
        escalation_required: false,
        diff_preview: None,
    };
    let s = draw(&base(ActiveRegion::Gate(card)));
    assert!(s.contains("auth/session.rs"));
    assert!(s.contains("+47 loc"));
    assert!(s.contains("cast — sealed"));
    // The sealed key must not reveal a verdict value.
    assert!(!s.contains("■ GO"));
    assert!(!s.contains("■ NO-GO"));
    assert!(s.contains("[g] go"));
}

#[test]
fn session_close_shows_verified_chain() {
    let summary = AuditSummary { gates: 4, reviewers: vec!["A · YOU".into(), "B · J.REED".into()], chain_verified: true, merged: true };
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
    }));
    assert!(s.contains("PLAN GATE"));
    assert!(s.contains("[y] approve plan"));
}

#[test]
fn dropped_link_shows_b_station_dropped() {
    let mut view = base(ActiveRegion::Idle);
    view.status.linked = false;
    let s = draw(&view);
    assert!(s.contains("B-STATION DROPPED"));
}

#[test]
fn render_diff_contains_diff_text_and_close_hint() {
    use kontur_tui::render::render_diff;
    let diff_text = "diff --git a/foo.rs b/foo.rs\n+fn added() {}";
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 30)).unwrap();
    terminal.draw(|f| render_diff(f, "gate-001", diff_text)).unwrap();
    let s = buf_string(terminal.backend().buffer());
    assert!(s.contains("diff --git"));
    assert!(s.contains("[o] close diff"));
}

#[test]
fn session_close_no_longer_says_unanimous() {
    let summary = AuditSummary { gates: 4, reviewers: vec!["A".into()], chain_verified: true, merged: true };
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
