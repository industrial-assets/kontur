//! End-to-end session test:
//! real git repo → GitWorkspace → GateHost → SessionServer on a real TCP listener
//! → two SessionClients connect, step through the protocol, ScriptedAgent resolves
//! → session closes with chain_verified and the repo's main branch gains exactly one
//!   commit whose message contains both Reviewed-by trailers.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kontur_core::{Ed25519Signer, ReviewDepth, Signer};
use kontur_mcp::{GateHost, GitWorkspace, SessionContext};
use kontur_net::{
    ScriptedAgent, ScriptedTask, SessionClient, SessionConfig, SessionServer,
    WirePhase, generate_tls, attach_tls,
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
            .args(["-c", "commit.gpgsign=false"])
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

/// A cursor over the client's message stream that re-tests the last-seen
/// state before reading fresh messages.
///
/// The server broadcasts via a `watch` channel, so a lagging connection
/// receives a CONFLATED latest-state, and one state can satisfy several
/// consecutive predicates. A naive "read until match" consumes such a state
/// and then waits forever for a message the (correctly) quiescent system will
/// never send. Every await must therefore first check the cached state.
struct StateCursor {
    rx: tokio::sync::mpsc::Receiver<kontur_net::ServerMsg>,
    last: Option<kontur_net::WireState>,
}

impl StateCursor {
    fn new(rx: tokio::sync::mpsc::Receiver<kontur_net::ServerMsg>) -> Self {
        StateCursor { rx, last: None }
    }

    async fn await_matching<F>(&mut self, label: &str, pred: F) -> kontur_net::WireState
    where
        F: Fn(&kontur_net::WireState) -> bool,
    {
        // Re-test the conflated last state first.
        if let Some(ws) = &self.last {
            if pred(ws) {
                return ws.clone();
            }
        }
        let mut seen: Vec<String> = Vec::new();
        loop {
            // Generous: a green run never waits this long; it only bounds
            // failure time.
            let msg = tokio::time::timeout(Duration::from_secs(55), self.rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("[{label}] timed out waiting for state; saw: {seen:?}")
                })
                .expect("channel closed unexpectedly");
            match msg {
                kontur_net::ServerMsg::State(ws) => {
                    let ws = *ws;
                    let matched = pred(&ws);
                    if !matched {
                        seen.push(format!(
                            "State(phase={:?}, gate={}, log_tail={:?})",
                            std::mem::discriminant(&ws.phase),
                            ws.gate.is_some(),
                            ws.log.last()
                        ));
                    }
                    self.last = Some(ws.clone());
                    if matched {
                        return ws;
                    }
                }
                other => seen.push(format!("{other:?}")),
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

        // Generate per-session TLS.
        let session_tls = generate_tls();
        let fingerprint = session_tls.fingerprint16();
        let acceptor = session_tls.acceptor.clone();

        // Accept-loop task.
        {
            let server_clone = server.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = op_listener.accept().await else { break };
                    attach_tls(&server_clone, &acceptor, stream).await;
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

        // --- 4. Two clients connect via TLS ------------------------------------
        let addr_str = op_addr.to_string();

        let (client_a, rx_a) =
            SessionClient::connect_pinned_tls(&addr_str, "A".into(), seed_a, fingerprint)
                .await
                .expect("client A connect failed");

        let (client_b, rx_b) =
            SessionClient::connect_pinned_tls(&addr_str, "B".into(), seed_b, fingerprint)
                .await
                .expect("client B connect failed");

        let mut cur_a = StateCursor::new(rx_a);
        let mut cur_b = StateCursor::new(rx_b);

        // --- 5. Step through protocol ------------------------------------------

        // Both connected → wait for DispatchReady.
        cur_a.await_matching("A:dispatch-ready", |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;
        cur_b.await_matching("B:dispatch-ready", |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;

        // Both ready → PlanReview.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        cur_a.await_matching("A:plan-review", |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        })
        .await;
        cur_b.await_matching("B:plan-review", |s| {
            matches!(s.phase, WirePhase::PlanReview { .. })
        })
        .await;

        // Both ready → Executing.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        cur_a.await_matching("A:executing", |s| matches!(s.phase, WirePhase::Executing)).await;
        cur_b.await_matching("B:executing", |s| matches!(s.phase, WirePhase::Executing)).await;

        // --- 6. Wait for a gate ------------------------------------------------
        let state_with_gate =
            cur_a.await_matching("A:gate-appears", |s| s.gate.is_some()).await;
        let wire_gate = state_with_gate.gate.unwrap();

        // --- 7. A casts go; assert B sees A's key as Sealed --------------------
        client_a.cast_go(&wire_gate, ReviewDepth::FullDiff).await.unwrap();

        let state_after_a = cur_b.await_matching("B:sees-A-sealed", |s| {
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
        client_b.cast_go(&wire_gate_b, ReviewDepth::FullDiff).await.unwrap();

        let closed_state = cur_a.await_matching("A:closed", |s| {
            matches!(s.phase, WirePhase::Closed { chain_verified: true, .. })
        })
        .await;

        match &closed_state.phase {
            WirePhase::Closed { chain_verified, merged, .. } => {
                assert!(chain_verified, "audit chain must be verified after close");
                // merged=true carries the actual merge_session result, so seeing
                // it guarantees the git merge completed before we inspect below.
                assert!(merged, "session close must report a successful merge");
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
