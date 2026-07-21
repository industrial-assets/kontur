use kontur_core::{Verdict, VerdictStatus};
use kontur_mcp::{GateHost, GateView};

use crate::fleet::FleetSource;
use crate::view::{
    ActiveRegion, AuditSummary, Banner, FileDiffView, GateCard, KeyStatus, KeyView, LogLine,
    SessionView, Station, StatusStrip,
};

/// Build the pure console snapshot. Pure w.r.t. the host + fleet at call time;
/// blind sealing is preserved because keys come only from `GateView.observed`.
pub async fn build_session_view(
    host: &GateHost,
    fleet: &dyn FleetSource,
    stations: [Station; 2],
    banner: Banner,
    log: Vec<LogLine>,
    closed: bool,
) -> SessionView {
    let agents = fleet.agents();
    let pending = host.pending_gates().await;

    let status = StatusStrip {
        linked: true,
        four_eyes: true,
        fleet_count: agents.len(),
        needs_you: pending.len(),
        tokens: agents.iter().map(|a| a.tokens).sum(),
    };

    let active = if closed {
        ActiveRegion::SessionClosed(AuditSummary {
            gates: host.audit_len().await,
            reviewers: stations.iter().map(|s| s.label.clone()).collect(),
            chain_verified: host.verify_audit().await.is_ok(),
            merged: true, // in-memory demo: acceptance is recorded; no git merge needed
            abandoned: false,
        })
    } else if let Some(gv) = pending.first() {
        let file_diffs = host
            .gate_diff(&gv.gate_id)
            .await
            .map(|b| {
                let fallback = match gv.files.as_slice() {
                    [only] => only.clone(),
                    _ => "(all files)".to_owned(),
                };
                kontur_net::difftext::split_file_diffs_or_whole(
                    &String::from_utf8_lossy(&b),
                    &fallback,
                )
                .into_iter()
                .map(|fd| FileDiffView {
                    path: fd.path,
                    diff: fd.diff,
                    truncated: false,
                })
                .collect()
            })
            .unwrap_or_default();
        ActiveRegion::Gate(gate_card(gv, &stations, file_diffs))
    } else {
        ActiveRegion::Idle
    };

    SessionView {
        banner,
        status,
        stations,
        fleet: agents,
        log,
        active,
        invite: None,
        notice: None,
        attention: None,
    }
}

fn gate_card(gv: &GateView, stations: &[Station; 2], file_diffs: Vec<FileDiffView>) -> GateCard {
    let keys = stations.iter().map(|s| key_for(s, gv)).collect();
    GateCard {
        gate_id: gv.gate_id.0.clone(),
        task: gv.task_id.0.clone(),
        files: gv.files.clone(),
        loc: gv.loc,
        keys,
        escalation_required: gv.escalation_required,
        file_diffs,
        // viewmodel path has no wire cap, so truncation is not applicable.
        diff_truncated: false,
    }
}

/// Derive a station's key status from the sealing-safe observed verdicts.
fn key_for(station: &Station, gv: &GateView) -> KeyView {
    let status = gv
        .observed
        .iter()
        .find(|v| v.operator == station.operator)
        .map(|v| match &v.status {
            VerdictStatus::Sealed => KeyStatus::Sealed,
            VerdictStatus::Revealed(Verdict::Go) => KeyStatus::Go,
            VerdictStatus::Revealed(Verdict::NoGo(_)) => KeyStatus::NoGo,
        })
        .unwrap_or(KeyStatus::Awaiting);
    KeyView {
        label: station.label.clone(),
        role: station.role,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::MockFleet;
    use crate::view::Role;
    use kontur_core::{CastVerdict, Ed25519Signer, FixedClock, ReviewDepth, Signer, TaskId};
    use kontur_mcp::{InMemoryWorkspace, SessionContext, Workspace};
    use std::sync::Arc;

    fn stations(a: kontur_core::OperatorId, b: kontur_core::OperatorId) -> [Station; 2] {
        [
            Station {
                label: "A · YOU".into(),
                role: Role::Host,
                activity: "watching".into(),
                operator: a,
            },
            Station {
                label: "B · J.REED".into(),
                role: Role::Operator,
                activity: "reviewing".into(),
                operator: b,
            },
        ]
    }

    fn banner() -> Banner {
        Banner {
            session: "4417".into(),
            version: "0.1.0".into(),
        }
    }

    #[tokio::test]
    async fn pending_gate_shows_gate_region_with_sealed_first_key() {
        let s1 = Ed25519Signer::from_seed([1; 32]);
        let s2 = Ed25519Signer::from_seed([2; 32]);
        let (op1, op2) = (s1.operator_id(), s2.operator_id());
        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new("do it", op1, "agent-01", "claude", "1.0", vec![op1, op2]);
        let host = GateHost::new(ctx, ws.clone());

        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (gid, _rx) = host.begin_task_gate(task, 0).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;

        // Station A casts (blind, sealed).
        let cv = CastVerdict::create(
            &s1,
            &FixedClock(1000),
            &gid,
            dh,
            kontur_core::Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        );
        host.submit_verdict(&gid, cv).await.unwrap();

        let view = build_session_view(
            &host,
            &MockFleet::demo(),
            stations(op1, op2),
            banner(),
            vec![],
            false,
        )
        .await;
        match view.active {
            ActiveRegion::Gate(card) => {
                assert_eq!(card.files, vec!["a.rs".to_string()]);
                assert_eq!(card.keys[0].status, KeyStatus::Sealed); // A cast, sealed
                assert_eq!(card.keys[1].status, KeyStatus::Awaiting); // B not yet
            }
            other => panic!("expected Gate, got {other:?}"),
        }
        assert_eq!(view.status.needs_you, 1);
    }

    #[tokio::test]
    async fn closed_shows_session_summary_with_verified_chain() {
        let s1 = Ed25519Signer::from_seed([1; 32]);
        let s2 = Ed25519Signer::from_seed([2; 32]);
        let (op1, op2) = (s1.operator_id(), s2.operator_id());
        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new("do it", op1, "agent-01", "claude", "1.0", vec![op1, op2]);
        let host = GateHost::new(ctx, ws.clone());

        let task = TaskId("t1".into());
        ws.apply_write(&task, "a.rs", b"x\n").unwrap();
        let (gid, _rx) = host.begin_task_gate(task, 0).await.unwrap();
        let dh = host.pending_gates().await[0].diff_hash;
        for s in [&s1, &s2] {
            let cv = CastVerdict::create(
                s,
                &FixedClock(1000),
                &gid,
                dh,
                kontur_core::Verdict::Go,
                ReviewDepth::FullDiff,
                None,
            );
            host.submit_verdict(&gid, cv).await.unwrap();
        }

        let view = build_session_view(
            &host,
            &MockFleet::demo(),
            stations(op1, op2),
            banner(),
            vec![],
            true,
        )
        .await;
        match view.active {
            ActiveRegion::SessionClosed(summary) => {
                assert_eq!(summary.gates, 1);
                assert!(summary.chain_verified);
                assert_eq!(summary.reviewers.len(), 2);
                assert!(summary.merged, "in-memory demo should report merged=true");
            }
            other => panic!("expected SessionClosed, got {other:?}"),
        }
    }
}
