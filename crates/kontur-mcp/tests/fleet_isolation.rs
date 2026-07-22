//! End-to-end: the `GateHost` `assign_task` hook routes two agents' work to
//! separate `FleetWorkspace` worktrees, so their gated diffs stay isolated even
//! though both touch the same file path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kontur_core::{Ed25519Signer, Signer, TaskId};
use kontur_mcp::{FleetWorkspace, GateHost, SessionContext};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
}

fn temp_repo(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("kontur-fleet-e2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    git(&p, &["init", "-b", "main"]);
    git(&p, &["config", "user.email", "t@kontur.local"]);
    git(&p, &["config", "user.name", "Kontur Test"]);
    std::fs::write(p.join("README.md"), "seed\n").unwrap();
    git(&p, &["add", "-A"]);
    git(&p, &["commit", "-m", "seed"]);
    p
}

#[tokio::test]
async fn two_agents_gates_are_isolated_through_gatehost() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let repo = temp_repo("gate");
    let ws = Arc::new(FleetWorkspace::create(repo, "s1", "agent-a").unwrap());
    let ctx = SessionContext::new("fleet", op1, "agent-a", "claude", "1.0", vec![op1, op2]);
    let host = GateHost::new(ctx, ws);

    // Two agents write to the SAME file path under their own namespaced tasks.
    let ta = TaskId("agent-a::1".into());
    let tb = TaskId("agent-b::1".into());
    host.record_write("agent-a", &ta, "shared.rs", b"from A\n")
        .await
        .unwrap();
    host.record_write("agent-b", &tb, "shared.rs", b"from B\n")
        .await
        .unwrap();

    // Each agent opens its own gate.
    host.begin_task_gate("agent-a", ta.clone(), 0)
        .await
        .unwrap();
    host.begin_task_gate("agent-b", tb.clone(), 0)
        .await
        .unwrap();

    let pending = host.pending_gates().await;
    assert_eq!(pending.len(), 2, "one gate per agent");

    for gate in &pending {
        let diff = host.gate_diff(&gate.gate_id).await.expect("diff bytes");
        let txt = String::from_utf8(diff).unwrap();
        match gate.agent.as_str() {
            "agent-a" => {
                assert!(txt.contains("from A"), "agent-a gate shows A's write");
                assert!(
                    !txt.contains("from B"),
                    "agent-a gate must NOT show agent-b's write"
                );
            }
            "agent-b" => {
                assert!(txt.contains("from B"), "agent-b gate shows B's write");
                assert!(
                    !txt.contains("from A"),
                    "agent-b gate must NOT show agent-a's write"
                );
            }
            other => panic!("unexpected agent attribution: {other}"),
        }
    }
}
