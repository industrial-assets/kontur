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
            Station { label: "A · YOU".into(), role: Role::Driver, activity: "watching".into(), operator: OperatorId([1; 32]) },
            Station { label: "B · J.REED".into(), role: Role::Navigator, activity: "reviewing".into(), operator: OperatorId([2; 32]) },
        ],
        fleet: vec![],
        log: vec![],
        active,
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
            KeyView { label: "A · YOU".into(), role: Role::Driver, status: KeyStatus::Awaiting },
            KeyView { label: "B · J.REED".into(), role: Role::Navigator, status: KeyStatus::Sealed },
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
    let summary = AuditSummary { gates: 4, reviewers: vec!["A · YOU".into(), "B · J.REED".into()], chain_verified: true };
    let s = draw(&base(ActiveRegion::SessionClosed(summary)));
    assert!(s.contains("4 gates · unanimous"));
    assert!(s.contains("chain verified"));
    assert!(s.contains("Reviewed-by: A · YOU"));
}
