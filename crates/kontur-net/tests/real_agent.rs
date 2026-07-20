//! End-to-end session test: real-agent path over TCP.
//!
//! A real git repo → GitWorkspace → GateHost → SessionServer on a TCP listener.
//! A second TCP listener exposes the MCP agent endpoint.
//! An in-process rmcp client plays the role of the agent (no `claude` binary
//! required).  The test exercises propose_plan → operator approval → write_file
//! → propose_task_complete → dual sign-off → session close.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kontur_core::{Ed25519Signer, Signer};
use kontur_mcp::{GateHost, GitWorkspace, SessionContext};
use kontur_net::{
    SessionClient, SessionConfig, SessionServer, WirePhase,
    serve_agent_endpoint,
};
use rmcp::model::CallToolRequestParams;
use rmcp::ServiceExt;

// ---------------------------------------------------------------------------
// Temp-repo helper (mirrors kontur-mcp git_workspace tests and tests/e2e.rs)
// ---------------------------------------------------------------------------

fn temp_repo() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);

    let mut p = std::env::temp_dir();
    p.push(format!(
        "kontur-real-agent-{}-{}",
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
    run(&["config", "user.email", "test@kontur.real-agent"]);
    run(&["config", "user.name", "Kontur Real Agent"]);
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
// StateCursor — conflation-safe state poller (mirrors tests/e2e.rs)
// ---------------------------------------------------------------------------

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
async fn real_agent_over_tcp() {
    tokio::time::timeout(Duration::from_secs(60), async {
        // --- 1. Set up a real git repo + GitWorkspace --------------------------
        let repo = temp_repo();

        let seed_a: [u8; 32] = [10u8; 32];
        let seed_b: [u8; 32] = [20u8; 32];
        let op_a = Ed25519Signer::from_seed(seed_a).operator_id();
        let op_b = Ed25519Signer::from_seed(seed_b).operator_id();

        let session = format!("real-agent-{}", std::process::id());
        let ws = GitWorkspace::create(repo.clone(), &session)
            .expect("GitWorkspace::create failed");
        let ws = Arc::new(ws);

        let ctx = SessionContext::new(
            "real agent e2e prompt",
            op_a,
            "agent-01",
            "claude",
            "1.0",
            vec![op_a, op_b],
        );
        let host = Arc::new(GateHost::new(ctx, ws));

        // --- 2. SessionServer on a real TCP listener (operator endpoint) -------
        let op_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let op_addr = op_listener.local_addr().unwrap();

        // cfg.plan is intentionally empty — the agent will provide the plan via
        // propose_plan; the server must refuse both-ready until the agent plan arrives.
        let cfg = SessionConfig {
            prompt: "real agent e2e prompt".into(),
            plan: vec![],
            seats: [("A".into(), op_a), ("B".into(), op_b)],
        };
        let server = SessionServer::new(host.clone(), cfg);

        {
            let server_clone = server.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = op_listener.accept().await else { break };
                    server_clone.attach(stream).await;
                }
            });
        }

        // --- 3. Agent MCP endpoint on a second TCP listener -------------------
        let agent_listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agent_addr = agent_listener.local_addr().unwrap();

        {
            let host_clone = host.clone();
            tokio::spawn(async move {
                serve_agent_endpoint(agent_listener, host_clone).await;
            });
        }

        // --- 4. Two operator clients connect ----------------------------------
        let addr_str = op_addr.to_string();

        let (client_a, rx_a) =
            SessionClient::connect_tcp(&addr_str, "A".into(), seed_a)
                .await
                .expect("client A connect failed");
        let (client_b, rx_b) =
            SessionClient::connect_tcp(&addr_str, "B".into(), seed_b)
                .await
                .expect("client B connect failed");

        let mut cur_a = StateCursor::new(rx_a);
        let mut cur_b = StateCursor::new(rx_b);

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

        // --- 5. Spawn the "real agent": rmcp client over TCP ------------------
        // The agent calls propose_plan {tasks:["t1: add guard"]} which blocks
        // until both operators approve the plan.
        let agent_addr_str = agent_addr.to_string();

        let propose_plan_handle = tokio::spawn(async move {
            let stream =
                tokio::net::TcpStream::connect(&agent_addr_str).await.unwrap();
            let client = ().serve(stream).await.expect("agent rmcp handshake");

            // propose_plan blocks until approve_plan() is called by the server.
            let plan_args = serde_json::json!({ "tasks": ["t1: add guard"] })
                .as_object()
                .cloned()
                .unwrap();
            let result = client
                .call_tool(
                    CallToolRequestParams::new("propose_plan")
                        .with_arguments(plan_args),
                )
                .await
                .expect("propose_plan call failed");

            // Return the client so we can continue using it for subsequent calls.
            (client, result)
        });

        // --- 6. Wait for PlanReview to show the agent's task string -----------
        cur_a
            .await_matching("A:plan-has-agent-task", |s| match &s.phase {
                WirePhase::PlanReview { tasks } => {
                    tasks.iter().any(|t| t.contains("t1: add guard"))
                }
                _ => false,
            })
            .await;

        cur_b
            .await_matching("B:plan-has-agent-task", |s| match &s.phase {
                WirePhase::PlanReview { tasks } => {
                    tasks.iter().any(|t| t.contains("t1: add guard"))
                }
                _ => false,
            })
            .await;

        // --- 7. Both seats approve the plan -----------------------------------
        // Both ready → server calls approve_plan() which unblocks propose_plan.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        cur_a.await_matching("A:executing", |s| matches!(s.phase, WirePhase::Executing)).await;
        cur_b.await_matching("B:executing", |s| matches!(s.phase, WirePhase::Executing)).await;

        // --- 8. Retrieve the rmcp client after propose_plan unblocks ----------
        let (agent_client, plan_result) =
            tokio::time::timeout(Duration::from_secs(10), propose_plan_handle)
                .await
                .expect("propose_plan handle timed out")
                .expect("propose_plan task panicked");

        assert_eq!(
            plan_result.is_error,
            Some(false),
            "propose_plan must succeed: {:?}",
            plan_result
        );

        // --- 9. Agent: write_file + propose_task_complete (both blocking) -----
        let write_args =
            serde_json::json!({ "task_id": "t1", "path": "src/guard.rs", "contents": "// guard\npub fn guard() {}\n" })
                .as_object()
                .cloned()
                .unwrap();
        agent_client
            .call_tool(
                CallToolRequestParams::new("write_file")
                    .with_arguments(write_args),
            )
            .await
            .expect("write_file call failed");

        // propose_task_complete blocks until both operators cast go.
        let propose_complete_handle = {
            let client2 = agent_client.clone();
            tokio::spawn(async move {
                let propose_args =
                    serde_json::json!({ "task_id": "t1", "tokens": 42 })
                        .as_object()
                        .cloned()
                        .unwrap();
                client2
                    .call_tool(
                        CallToolRequestParams::new("propose_task_complete")
                            .with_arguments(propose_args),
                    )
                    .await
            })
        };

        // --- 10. Wait for the gate to appear ----------------------------------
        let state_with_gate =
            cur_a.await_matching("A:gate-appears", |s| s.gate.is_some()).await;
        let wire_gate = state_with_gate.gate.unwrap();

        // --- 11. A casts go; assert B sees A's key as Sealed ------------------
        client_a.cast_go(&wire_gate).await.unwrap();

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

        // --- 12. B casts go → gate resolves; propose_task_complete unblocks ---
        let wire_gate_b = state_after_a.gate.unwrap();
        client_b.cast_go(&wire_gate_b).await.unwrap();

        let complete_result =
            tokio::time::timeout(Duration::from_secs(10), propose_complete_handle)
                .await
                .expect("propose_task_complete handle timed out")
                .expect("propose_task_complete task panicked")
                .expect("propose_task_complete call failed");

        assert_eq!(
            complete_result.is_error,
            Some(false),
            "propose_task_complete must succeed"
        );

        // Agent client is done; dropping it closes the TCP connection.
        drop(agent_client);

        // --- 13. Signal agent_done so the server can finalise -----------------
        // In production the bin wires child-exit → agent_done. In tests we call
        // it directly (Task 3 scope).
        server.agent_done().await;

        // --- 14. Wait for Closed with chain_verified and merged ---------------
        let closed_state = cur_a
            .await_matching("A:closed", |s| {
                matches!(s.phase, WirePhase::Closed { chain_verified: true, .. })
            })
            .await;

        match &closed_state.phase {
            WirePhase::Closed { chain_verified, merged, .. } => {
                assert!(chain_verified, "audit chain must be verified after close");
                assert!(merged, "session close must report a successful merge");
            }
            _ => panic!("expected Closed phase"),
        }

        // Brief yield so the git merge completes before we inspect the repo.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // --- 15. Assert repo: one new commit with both Reviewed-by trailers ---
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
    .expect("real_agent_over_tcp test timed out after 60 seconds");
}
