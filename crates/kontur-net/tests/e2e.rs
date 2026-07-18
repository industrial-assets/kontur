//! End-to-end session test:
//! real git repo → GitWorkspace → GateHost → SessionServer on a real TCP listener
//! → two SessionClients connect, step through the protocol, ScriptedAgent resolves
//! → session closes with chain_verified and the repo's main branch gains exactly one
//!   commit whose message contains both Reviewed-by trailers.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kontur_core::{Ed25519Signer, Signer};
use kontur_mcp::{GateHost, GitWorkspace, SessionContext};
use kontur_net::{
    ScriptedAgent, ScriptedTask, SessionClient, SessionConfig, SessionServer,
    WirePhase,
};

// ---------------------------------------------------------------------------
// Temp-repo helper (mirrors kontur-mcp git_workspace tests)
// ---------------------------------------------------------------------------

fn temp_repo() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);

    let mut p = std::env::temp_dir();
    p.push(format!(
        "kontur-e2e-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&p)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@kontur.e2e"]);
    run(&["config", "user.name", "Kontur E2E"]);
    std::fs::write(p.join("README.md"), "seed\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-m", "seed"]);
    p
}

fn git_log_latest(repo: &PathBuf) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%B", "main"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_commit_count(repo: &PathBuf) -> usize {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", "main"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Helpers: advance to state matching predicate via mpsc
// ---------------------------------------------------------------------------

async fn next_state_matching<F>(
    rx: &mut tokio::sync::mpsc::Receiver<kontur_net::ServerMsg>,
    pred: F,
) -> kontur_net::WireState
where
    F: Fn(&kontur_net::WireState) -> bool,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for state message")
            .expect("channel closed unexpectedly");
        if let kontur_net::ServerMsg::State(ws) = msg {
            if pred(&ws) {
                return *ws;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_two_clients_scripted_agent_real_tcp_git() {
    tokio::time::timeout(Duration::from_secs(60), async {
        // --- 1. Set up a real git repo + GitWorkspace ---------------------------
        let repo = temp_repo();

        let seed_a: [u8; 32] = [10u8; 32];
        let seed_b: [u8; 32] = [20u8; 32];
        let op_a = Ed25519Signer::from_seed(seed_a).operator_id();
        let op_b = Ed25519Signer::from_seed(seed_b).operator_id();

        // Use a unique session name to avoid worktree path collisions.
        let session = format!("e2e-{}", std::process::id());

        let ws = GitWorkspace::create(repo.clone(), &session)
            .expect("GitWorkspace::create failed");
        let ws = Arc::new(ws);

        let ctx = SessionContext::new(
            "e2e test prompt",
            op_a,
            "agent-01",
            "external",
            "1.0",
            vec![op_a, op_b],
        );
        let host = Arc::new(GateHost::new(ctx, ws));

        // --- 2. SessionServer on a real TCP listener ---------------------------
        let op_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let op_addr = op_listener.local_addr().unwrap();

        let cfg = SessionConfig {
            prompt: "e2e test prompt".into(),
            plan: vec!["task-1: add e2e.rs".into()],
            seats: [("A".into(), op_a), ("B".into(), op_b)],
        };
        let server = SessionServer::new(host.clone(), cfg);

        // Accept-loop task.
        {
            let server_clone = server.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = op_listener.accept().await else { break };
                    server_clone.attach(stream).await;
                }
            });
        }

        // --- 3. Scripted agent (1 task) ----------------------------------------
        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/e2e.rs".into(),
                contents: "// e2e\npub fn e2e() {}\n".into(),
            }],
        };
        {
            let server_clone = server.clone();
            tokio::spawn(async move { agent.run(server_clone).await });
        }

        // --- 4. Two clients connect via TCP ------------------------------------
        let addr_str = op_addr.to_string();

        let (client_a, mut rx_a) =
            SessionClient::connect_tcp(&addr_str, "A".into(), seed_a)
                .await
                .expect("client A connect failed");

        let (client_b, mut rx_b) =
            SessionClient::connect_tcp(&addr_str, "B".into(), seed_b)
                .await
                .expect("client B connect failed");

        // --- 5. Step through protocol ------------------------------------------

        // Both connected → wait for DispatchReady.
        next_state_matching(&mut rx_a, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;
        next_state_matching(&mut rx_b, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;

        // Both ready → PlanReview.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        next_state_matching(&mut rx_a, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        })
        .await;
        next_state_matching(&mut rx_b, |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        })
        .await;

        // Both ready → Executing.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        next_state_matching(&mut rx_a, |s| matches!(s.phase, WirePhase::Executing)).await;
        next_state_matching(&mut rx_b, |s| matches!(s.phase, WirePhase::Executing)).await;

        // --- 6. Wait for a gate ------------------------------------------------
        let state_with_gate =
            next_state_matching(&mut rx_a, |s| s.gate.is_some()).await;
        let wire_gate = state_with_gate.gate.unwrap();

        // --- 7. A casts go; assert B sees A's key as Sealed --------------------
        client_a.cast_go(&wire_gate).await.unwrap();

        let state_after_a = next_state_matching(&mut rx_b, |s| {
            s.gate
                .as_ref()
                .map(|g| !g.keys.is_empty())
                .unwrap_or(false)
        })
        .await;

        let gate_b_view = state_after_a.gate.as_ref().unwrap();
        assert!(
            gate_b_view
                .keys
                .iter()
                .any(|k| k.status == kontur_core::VerdictStatus::Sealed),
            "A's key should be Sealed on B's view before B votes"
        );

        // --- 8. B casts go; await Closed with chain_verified -------------------
        let wire_gate_b = state_after_a.gate.unwrap();
        client_b.cast_go(&wire_gate_b).await.unwrap();

        let closed_state = next_state_matching(&mut rx_a, |s| {
            matches!(s.phase, WirePhase::Closed { chain_verified: true, .. })
        })
        .await;

        match &closed_state.phase {
            WirePhase::Closed { chain_verified, .. } => {
                assert!(chain_verified, "audit chain must be verified after close");
            }
            _ => panic!("expected Closed phase"),
        }

        // Brief yield so the server's finalize task (which does the git merge)
        // completes before we inspect the repo.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // --- 9. Assert repo: exactly one new commit on main with both Reviewed-by --
        let count = git_commit_count(&repo);
        // seed commit (1) + merge commit (2)
        assert_eq!(
            count, 2,
            "expected exactly 2 commits on main (seed + merged session), got {count}"
        );

        let log = git_log_latest(&repo);
        assert!(
            log.contains("Reviewed-by: A"),
            "merge commit must contain 'Reviewed-by: A'; got: {log:?}"
        );
        assert!(
            log.contains("Reviewed-by: B"),
            "merge commit must contain 'Reviewed-by: B'; got: {log:?}"
        );
    })
    .await
    .expect("e2e test timed out after 60 seconds");
}
