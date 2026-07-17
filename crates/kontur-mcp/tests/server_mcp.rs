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
    CastVerdict::create(&signer, &FixedClock(1000 + seed as i64), gate_id, dh, Verdict::Go, ReviewDepth::FullDiff, None)
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
    let ctx = SessionContext::new("refactor guard", op1, "agent-01", "claude", "1.0", vec![op1, op2]);
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
    let write_args = serde_json::json!({ "task_id": "t1", "path": "a.rs", "contents": "guarded\n" })
        .as_object()
        .cloned()
        .unwrap();
    client
        .call_tool(CallToolRequestParams::new("write_file").with_arguments(write_args))
        .await
        .expect("write_file call");
    assert_eq!(ws.file_contents(&TaskId("t1".into()), "a.rs"), Some(b"guarded\n".to_vec()));

    // 2) propose_task_complete — blocks; drive it on a task.
    let propose_args = serde_json::json!({ "task_id": "t1", "tokens": 42 })
        .as_object()
        .cloned()
        .unwrap();
    let client2 = client.clone();
    let propose = tokio::spawn(async move {
        client2
            .call_tool(CallToolRequestParams::new("propose_task_complete").with_arguments(propose_args))
            .await
    });

    // 3) Two operators sign off via the operator face.
    let (gate_id, dh) = wait_for_gate(&host).await;
    host.submit_verdict(&gate_id, go(1, &gate_id, dh)).await.unwrap();
    host.submit_verdict(&gate_id, go(2, &gate_id, dh)).await.unwrap();

    // 4) The blocked tool call now returns success, and the audit chain holds.
    let result = propose.await.expect("join").expect("propose call ok");
    assert_eq!(result.is_error, Some(false));
    assert!(host.verify_audit().await.is_ok());
    assert_eq!(ws.accepted_tasks(), vec![TaskId("t1".into())]);
    assert_eq!(host.reviewed_by(&gate_id).await.unwrap().len(), 2);
}
