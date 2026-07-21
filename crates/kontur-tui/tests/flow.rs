use kontur_tui::demo::Demo;
use kontur_tui::view::ActiveRegion;
use kontur_tui::{build_session_view, MockFleet};

#[tokio::test]
async fn full_flow_pending_to_accepted_audited() {
    let demo = Demo::new();
    let (gid, dh) = demo.open_demo_gate().await;

    // Before verdicts: a pending gate is the active region.
    let view = build_session_view(
        demo.host(),
        &MockFleet::demo(),
        demo.stations(),
        demo.banner(),
        vec![],
        false,
    )
    .await;
    assert!(matches!(view.active, ActiveRegion::Gate(_)));

    // Station A casts, then the scripted second key.
    demo.host()
        .submit_verdict(&gid, demo.go_a(&gid, dh))
        .await
        .unwrap();
    demo.host()
        .submit_verdict(&gid, demo.go_b(&gid, dh))
        .await
        .unwrap();

    // Closed: verified audit, two reviewers, one gate.
    let view = build_session_view(
        demo.host(),
        &MockFleet::demo(),
        demo.stations(),
        demo.banner(),
        vec![],
        true,
    )
    .await;
    match view.active {
        ActiveRegion::SessionClosed(summary) => {
            assert_eq!(summary.gates, 1);
            assert!(summary.chain_verified);
            assert_eq!(summary.reviewers.len(), 2);
        }
        other => panic!("expected SessionClosed, got {other:?}"),
    }
    assert!(demo.host().verify_audit().await.is_ok());
}
