use std::sync::Arc;
use std::time::Duration;

use kontur_core::{
    CastVerdict, Ed25519Signer, FixedClock, GateId, Hash, ReviewDepth, Signer, TaskId, Verdict,
};
use kontur_mcp::{GateHost, InMemoryWorkspace, KonturServer, SessionContext};

use rmcp::model::CallToolRequestParams;
use rmcp::{serve_server, ServiceExt};

fn go(seed: u8, gate_id: &GateId, dh: Hash) -> CastVerdict {
    let signer = Ed25519Signer::from_seed([seed; 32]);
    CastVerdict::create(
        &signer,
        &FixedClock(1000 + seed as i64),
        gate_id,
        dh,
        Verdict::Go,
        ReviewDepth::FullDiff,
        None,
    )
}

/// Poll the operator face until a gate appears (the agent-side handler opens it
/// asynchronously). Bounded so a bug fails fast instead of hanging.
async fn wait_for_gate(host: &GateHost) -> (GateId, Hash) {
    for _ in 0..2000 {
        if let Some(v) = host.pending_gates().await.into_iter().next() {
            return (v.gate_id, v.diff_hash);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("no gate appeared");
}

#[tokio::test]
async fn agent_write_then_propose_gated_by_two_operators() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new(
        "refactor guard",
        op1,
        "agent-01",
        "claude",
        "1.0",
        vec![op1, op2],
    );
    let host = Arc::new(GateHost::new(ctx, ws.clone()));

    // Wire an in-process client<->server over a duplex pipe.
    let (server_io, client_io) = tokio::io::duplex(8192);

    let server = KonturServer::new(host.clone());
    tokio::spawn(async move {
        if let Ok(running) = serve_server(server, server_io).await {
            // Keep the server alive until the client disconnects.
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client handshake");

    // 1) write_file — ungated, executes in the workspace.
    let write_args =
        serde_json::json!({ "task_id": "t1", "path": "a.rs", "contents": "guarded\n" })
            .as_object()
            .cloned()
            .unwrap();
    client
        .call_tool(CallToolRequestParams::new("write_file").with_arguments(write_args))
        .await
        .expect("write_file call");
    // The connection's agent id (default "agent-01") namespaces the task.
    assert_eq!(
        ws.file_contents(&TaskId("agent-01::t1".into()), "a.rs"),
        Some(b"guarded\n".to_vec())
    );

    // 2) propose_task_complete — blocks; drive it on a task.
    let propose_args = serde_json::json!({ "task_id": "t1", "tokens": 42 })
        .as_object()
        .cloned()
        .unwrap();
    let client2 = client.clone();
    let propose = tokio::spawn(async move {
        client2
            .call_tool(
                CallToolRequestParams::new("propose_task_complete").with_arguments(propose_args),
            )
            .await
    });

    // 3) Two operators sign off via the operator face.
    let (gate_id, dh) = wait_for_gate(&host).await;
    host.submit_verdict(&gate_id, go(1, &gate_id, dh))
        .await
        .unwrap();
    host.submit_verdict(&gate_id, go(2, &gate_id, dh))
        .await
        .unwrap();

    // 4) The blocked tool call now returns success, and the audit chain holds.
    let result = propose.await.expect("join").expect("propose call ok");
    assert_eq!(result.is_error, Some(false));
    assert!(host.verify_audit().await.is_ok());
    assert_eq!(ws.accepted_tasks(), vec![TaskId("agent-01::t1".into())]);
    assert_eq!(host.reviewed_by(&gate_id).await.unwrap().len(), 2);
}

/// Two agents (distinct MCP connections) that independently pick the SAME
/// logical task id ("1") must not collide: each connection's authoritative
/// agent id namespaces the task, so their writes land in separate tasks.
#[tokio::test]
async fn two_agents_same_task_id_are_isolated() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new("fleet", op1, "agent-01", "claude", "1.0", vec![op1, op2]);
    let host = Arc::new(GateHost::new(ctx, ws.clone()));

    async fn connect(
        host: Arc<GateHost>,
        agent: &str,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (server_io, client_io) = tokio::io::duplex(8192);
        let server = KonturServer::with_agent(host, agent);
        tokio::spawn(async move {
            if let Ok(running) = serve_server(server, server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await.expect("client handshake")
    }

    let client_a = connect(host.clone(), "agent-a").await;
    let client_b = connect(host.clone(), "agent-b").await;

    // Both agents write to their own "task 1" with different files.
    let write = |tid: &str, path: &str, contents: &str| {
        serde_json::json!({ "task_id": tid, "path": path, "contents": contents })
            .as_object()
            .cloned()
            .unwrap()
    };
    client_a
        .call_tool(
            CallToolRequestParams::new("write_file").with_arguments(write("1", "a.rs", "A\n")),
        )
        .await
        .expect("agent-a write");
    client_b
        .call_tool(
            CallToolRequestParams::new("write_file").with_arguments(write("1", "b.rs", "B\n")),
        )
        .await
        .expect("agent-b write");

    // Each agent's write is isolated under its namespaced task.
    assert_eq!(
        ws.file_contents(&TaskId("agent-a::1".into()), "a.rs"),
        Some(b"A\n".to_vec())
    );
    assert_eq!(
        ws.file_contents(&TaskId("agent-b::1".into()), "b.rs"),
        Some(b"B\n".to_vec())
    );
    // Cross-contamination is impossible: agent-a's task never sees agent-b's file.
    assert_eq!(ws.file_contents(&TaskId("agent-a::1".into()), "b.rs"), None);
    assert_eq!(ws.file_contents(&TaskId("agent-b::1".into()), "a.rs"), None);
    // The un-namespaced bare id is never used as a task key.
    assert_eq!(ws.file_contents(&TaskId("1".into()), "a.rs"), None);
}

#[tokio::test]
async fn propose_plan_blocks_until_approved() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new(
        "plan gate test",
        op1,
        "agent-02",
        "claude",
        "1.0",
        vec![op1, op2],
    );
    let host = Arc::new(GateHost::new(ctx, ws));

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = KonturServer::new(host.clone());
    tokio::spawn(async move {
        if let Ok(running) = serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client handshake");

    let plan_args = serde_json::json!({ "tasks": ["add caching", "write tests"] })
        .as_object()
        .cloned()
        .unwrap();

    let client2 = client.clone();
    let propose = tokio::spawn(async move {
        client2
            .call_tool(CallToolRequestParams::new("propose_plan").with_arguments(plan_args))
            .await
    });

    // Poll until the plan is stored (the spawned task has called propose_plan on the host).
    for _ in 0..2000 {
        if host.proposed_plan().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        host.proposed_plan().await,
        Some(vec!["add caching".to_string(), "write tests".to_string()]),
        "plan must be stored before approve"
    );

    // Assert the tool call has NOT yet completed (plan not approved).
    assert!(
        !propose.is_finished(),
        "propose_plan must still be blocking"
    );

    // Approve — should unblock the tool call.
    host.approve_plan().await;

    let result = tokio::time::timeout(Duration::from_secs(5), propose)
        .await
        .expect("approve must unblock within 5s")
        .expect("task join")
        .expect("propose_plan tool call ok");

    assert_eq!(result.is_error, Some(false));
    // Verify the returned JSON contains approved:true.
    let content = &result.content[0];
    let text = match content {
        rmcp::model::ContentBlock::Text(t) => &t.text,
        other => panic!("unexpected content: {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(text).expect("valid json");
    assert_eq!(v["approved"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn propose_plan_steered_returns_error_then_approve_succeeds() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new(
        "plan steer test",
        op1,
        "agent-03",
        "claude",
        "1.0",
        vec![op1, op2],
    );
    let host = Arc::new(GateHost::new(ctx, ws));

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = KonturServer::new(host.clone());
    tokio::spawn(async move {
        if let Ok(running) = serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client handshake");

    // First proposal — will be steered.
    let plan_args = serde_json::json!({ "tasks": ["do everything at once"] })
        .as_object()
        .cloned()
        .unwrap();
    let client2 = client.clone();
    let propose = tokio::spawn(async move {
        client2
            .call_tool(CallToolRequestParams::new("propose_plan").with_arguments(plan_args))
            .await
    });

    // Wait until the plan is stored, then steer.
    for _ in 0..2000 {
        if host.proposed_plan().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    host.steer_plan("split task 2".to_string()).await;

    // The steered propose_plan call returns an MCP error carrying the steer.
    let err = tokio::time::timeout(Duration::from_secs(5), propose)
        .await
        .expect("steer must unblock within 5s")
        .expect("task join")
        .expect_err("steered plan must surface as an MCP error");
    let msg = err.to_string();
    assert!(
        msg.contains("split task 2"),
        "error must carry the steer: {msg}"
    );

    // Agent re-proposes; approve; success.
    let plan_args2 = serde_json::json!({ "tasks": ["task a", "task b"] })
        .as_object()
        .cloned()
        .unwrap();
    let client3 = client.clone();
    let propose2 = tokio::spawn(async move {
        client3
            .call_tool(CallToolRequestParams::new("propose_plan").with_arguments(plan_args2))
            .await
    });

    for _ in 0..2000 {
        if host.proposed_plan().await == Some(vec!["task a".to_string(), "task b".to_string()]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    host.approve_plan().await;

    let result2 = tokio::time::timeout(Duration::from_secs(5), propose2)
        .await
        .expect("approve must unblock within 5s")
        .expect("task join")
        .expect("propose_plan tool call ok");
    assert_eq!(
        result2.is_error,
        Some(false),
        "re-proposed plan must succeed after approve"
    );
    let text2 = match &result2.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("unexpected content: {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&text2).expect("valid json");
    assert_eq!(v["approved"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn ask_clarification_blocks_until_resolved() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new(
        "clarify test",
        op1,
        "agent-02",
        "claude",
        "1.0",
        vec![op1, op2],
    );
    let host = Arc::new(GateHost::new(ctx, ws));

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = KonturServer::new(host.clone());
    tokio::spawn(async move {
        if let Ok(running) = serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client handshake");

    let args = serde_json::json!({
        "questions": [
            { "prompt": "target db?", "options": ["postgres", "sqlite"] }
        ]
    })
    .as_object()
    .cloned()
    .unwrap();

    let client2 = client.clone();
    let ask = tokio::spawn(async move {
        client2
            .call_tool(CallToolRequestParams::new("ask_clarification").with_arguments(args))
            .await
    });

    // Wait until the questions are stored (the tool called ask_clarification).
    for _ in 0..2000 {
        if host.asked_questions().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        host.asked_questions().await.is_some(),
        "questions must be stored"
    );
    assert!(
        !ask.is_finished(),
        "ask_clarification must still be blocking"
    );

    // Resolve — unblocks the tool with the answers.
    host.resolve_clarification(vec![vec!["postgres".to_string()]])
        .await;

    let result = tokio::time::timeout(Duration::from_secs(5), ask)
        .await
        .expect("resolve must unblock within 5s")
        .expect("task join")
        .expect("ask_clarification tool call ok");
    assert_eq!(result.is_error, Some(false));
    let text = match &result.content[0] {
        rmcp::model::ContentBlock::Text(t) => &t.text,
        other => panic!("unexpected content: {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(text).expect("valid json");
    assert_eq!(
        v["answers"][0][0],
        serde_json::Value::String("postgres".into())
    );
}

#[tokio::test]
async fn propose_split_blocks_until_resolved() {
    let op1 = Ed25519Signer::from_seed([1; 32]).operator_id();
    let op2 = Ed25519Signer::from_seed([2; 32]).operator_id();
    let ws = Arc::new(InMemoryWorkspace::new());
    let ctx = SessionContext::new(
        "split test",
        op1,
        "agent-02",
        "claude",
        "1.0",
        vec![op1, op2],
    );
    let host = Arc::new(GateHost::new(ctx, ws));

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = KonturServer::new(host.clone());
    tokio::spawn(async move {
        if let Ok(running) = serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client handshake");

    let args = serde_json::json!({
        "streams": [
            { "title": "backend", "detail": "API" },
            { "title": "frontend", "detail": "UI" }
        ]
    })
    .as_object()
    .cloned()
    .unwrap();

    let client2 = client.clone();
    let ask = tokio::spawn(async move {
        client2
            .call_tool(CallToolRequestParams::new("propose_split").with_arguments(args))
            .await
    });

    for _ in 0..2000 {
        if host.proposed_split().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(host.proposed_split().await.is_some(), "streams stored");
    assert!(!ask.is_finished(), "propose_split must still be blocking");

    host.resolve_split(kontur_mcp::SplitDecision::Approved(
        host.proposed_split().await.unwrap(),
    ))
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), ask)
        .await
        .expect("resolve unblocks")
        .expect("join")
        .expect("propose_split ok");
    assert_eq!(result.is_error, Some(false));
    let text = match &result.content[0] {
        rmcp::model::ContentBlock::Text(t) => &t.text,
        other => panic!("unexpected content: {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(text).expect("valid json");
    assert_eq!(v["approved"], serde_json::Value::Bool(true));
}
