use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, Mutex};

use kontur_core::{
    CastVerdict, Clock, Ed25519Signer, OperatorId, Remedy, ReviewDepth, Signer, Timestamp, Verdict,
};

use crate::codec::{read_json, write_json};
use crate::protocol::{ClientMsg, ServerMsg, WireGate};

// ---------------------------------------------------------------------------
// SystemClock
// ---------------------------------------------------------------------------

/// Wall-clock time source: milliseconds since the Unix epoch.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        )
    }
}

// ---------------------------------------------------------------------------
// SessionClient
// ---------------------------------------------------------------------------

/// Client-side session handle. Holds the write half (behind a Mutex for
/// interior mutability without &mut self on public methods) and the operator's
/// signing key.  The private key never leaves the client process.
pub struct SessionClient {
    writer: Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
    signer: Ed25519Signer,
}

impl SessionClient {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Attach to an arbitrary async stream. Performs the Hello/Welcome
    /// handshake and spawns a reader task that forwards every `ServerMsg`
    /// into the returned `Receiver`.
    pub async fn attach<S>(
        stream: S,
        seat: String,
        seed: [u8; 32],
    ) -> io::Result<(SessionClient, mpsc::Receiver<ServerMsg>)>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let signer = Ed25519Signer::from_seed(seed);
        let operator = signer.operator_id();

        let (read_half, write_half) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(read_half);

        // Use a boxed write half so we can store it without a type parameter.
        let mut write_half: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = Box::new(write_half);

        // Send Hello.
        write_json(&mut write_half, &ClientMsg::Hello { seat, operator }).await?;

        // Create the mpsc channel before the reader task is spawned.
        let (tx, rx) = mpsc::channel::<ServerMsg>(32);

        // Read messages until Welcome arrives, forwarding any early State messages.
        loop {
            match read_json::<_, ServerMsg>(&mut buf_reader).await? {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "server closed connection before Welcome",
                    ));
                }
                Some(ServerMsg::Welcome { .. }) => {
                    // Handshake complete — break out and spawn the reader.
                    break;
                }
                Some(ServerMsg::Rejected { reason }) => {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
                }
                Some(other) => {
                    // Forward early State (or any other message) into the channel.
                    // If the receiver is already gone this is a no-op.
                    let _ = tx.send(other).await;
                }
            }
        }

        // Spawn reader task: forwards every subsequent ServerMsg until EOF.
        tokio::spawn(reader_task(buf_reader, tx));

        let client = SessionClient {
            writer: Mutex::new(write_half),
            signer,
        };

        Ok((client, rx))
    }

    /// Connect to a TCP endpoint without TLS. Only for in-process/loopback tests
    /// (e.g., tests using `attach` directly with duplex streams).
    /// Production operator connections use `connect_pinned_tls`.
    pub async fn connect_tcp_plain(
        addr: &str,
        seat: String,
        seed: [u8; 32],
    ) -> io::Result<(SessionClient, mpsc::Receiver<ServerMsg>)> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        Self::attach(stream, seat, seed).await
    }

    /// Connect to a TLS endpoint with cert pinning and attach.
    pub async fn connect_pinned_tls(
        addr: &str,
        seat: String,
        seed: [u8; 32],
        fp16: [u8; 16],
    ) -> io::Result<(SessionClient, mpsc::Receiver<ServerMsg>)> {
        let tls_stream = crate::tls::connect_pinned(addr, fp16).await?;
        Self::attach(tls_stream, seat, seed).await
    }

    // -----------------------------------------------------------------------
    // Identity
    // -----------------------------------------------------------------------

    pub fn operator(&self) -> OperatorId {
        self.signer.operator_id()
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    pub async fn ready(&self) -> io::Result<()> {
        self.send(ClientMsg::Ready).await
    }

    pub async fn hand_edit(&self, path: &str, contents: &str) -> io::Result<()> {
        self.send(ClientMsg::HandEdit {
            path: path.to_owned(),
            contents: contents.to_owned(),
        })
        .await
    }

    pub async fn abandon(&self) -> io::Result<()> {
        self.send(ClientMsg::Abandon).await
    }

    /// Send a prompt edit. Valid only during DispatchReady; the server will
    /// Reject it otherwise and reset both ready flags on acceptance.
    pub async fn set_prompt(&self, prompt: &str) -> io::Result<()> {
        self.send(ClientMsg::SetPrompt {
            prompt: prompt.to_owned(),
        })
        .await
    }

    /// Replace the plan with an edited task list. Valid only during PlanReview;
    /// the server will Reject it otherwise and reset both ready flags on acceptance.
    pub async fn edit_plan(&self, tasks: &[String]) -> io::Result<()> {
        self.send(ClientMsg::EditPlan {
            tasks: tasks.to_vec(),
        })
        .await
    }

    /// Send a plan steer to the agent. Valid only during PlanReview.
    pub async fn steer_plan(&self, steer: &str) -> io::Result<()> {
        self.send(ClientMsg::SteerPlan {
            steer: steer.to_owned(),
        })
        .await
    }

    /// Request the current worktree contents of a file. The response arrives
    /// as `ServerMsg::FileContent` on the normal server-message stream; the
    /// TUI correlates by path. Fire-and-forget: the caller reads the response
    /// from the receiver it obtained at `attach` time.
    pub async fn fetch_file(&self, path: &str) -> io::Result<()> {
        self.send(ClientMsg::FetchFile {
            path: path.to_owned(),
        })
        .await
    }

    /// Sign a Go verdict against the gate described by `wire_gate` and send it.
    pub async fn cast_go(&self, gate: &WireGate, depth: ReviewDepth) -> io::Result<()> {
        let verdict = self.build_verdict(gate, Verdict::Go, depth);
        self.send(ClientMsg::Cast {
            gate_id: gate.gate_id.clone(),
            verdict,
        })
        .await
    }

    /// Sign a NoGo verdict with a Steer remedy and send it.
    pub async fn cast_nogo(
        &self,
        gate: &WireGate,
        steer: &str,
        depth: ReviewDepth,
    ) -> io::Result<()> {
        let verdict =
            self.build_verdict(gate, Verdict::NoGo(Remedy::Steer(steer.to_owned())), depth);
        self.send(ClientMsg::Cast {
            gate_id: gate.gate_id.clone(),
            verdict,
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn build_verdict(&self, gate: &WireGate, v: Verdict, depth: ReviewDepth) -> CastVerdict {
        CastVerdict::create(
            &self.signer,
            &SystemClock,
            &gate.gate_id,
            gate.diff_hash,
            v,
            depth,
            None,
        )
    }

    async fn send(&self, msg: ClientMsg) -> io::Result<()> {
        let mut w = self.writer.lock().await;
        write_json(&mut *w, &msg).await
    }
}

// ---------------------------------------------------------------------------
// Reader task
// ---------------------------------------------------------------------------

async fn reader_task<R>(mut reader: BufReader<R>, tx: mpsc::Sender<ServerMsg>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    while let Ok(Some(msg)) = read_json::<_, ServerMsg>(&mut reader).await {
        if tx.send(msg).await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use kontur_core::{Ed25519Signer, ReviewDepth, Signer, VerdictStatus};
    use kontur_mcp::{GateHost, InMemoryWorkspace, SessionContext};

    use crate::agent::run_agent;
    use crate::protocol::{ServerMsg, WirePhase};
    use crate::server::ScriptedAgent;
    use crate::server::{ScriptedTask, SessionConfig, SessionServer};

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_server_and_clients() -> (
        SessionServer,
        Arc<InMemoryWorkspace>,
        [u8; 32], // seed A
        [u8; 32], // seed B
    ) {
        let seed_a: [u8; 32] = [1u8; 32];
        let seed_b: [u8; 32] = [2u8; 32];

        let op_a = Ed25519Signer::from_seed(seed_a).operator_id();
        let op_b = Ed25519Signer::from_seed(seed_b).operator_id();

        let ws = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new(
            "add auth gate",
            op_a,
            "agent-01",
            "claude",
            "1.0",
            vec![op_a, op_b],
        );
        let host = Arc::new(GateHost::new(ctx, ws.clone()));

        let cfg = SessionConfig {
            prompt: "add auth gate".into(),
            plan: vec!["auth.rs".into()],
            seats: [("A".into(), op_a), ("B".into(), op_b)],
        };

        let server = SessionServer::new(host, cfg);
        (server, ws, seed_a, seed_b)
    }

    /// Receive messages until the predicate is satisfied; time-bounded.
    async fn next_state_matching<F>(
        rx: &mut mpsc::Receiver<ServerMsg>,
        pred: F,
    ) -> crate::protocol::WireState
    where
        F: Fn(&crate::protocol::WireState) -> bool,
    {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timed out waiting for state")
                .expect("channel closed unexpectedly");
            if let ServerMsg::State(ws) = msg {
                if pred(&ws) {
                    return *ws;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Loopback test: happy arc + duplicate-cast rejection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn loopback_full_arc_and_duplicate_rejected() {
        let (server, _ws, seed_a, seed_b) = make_server_and_clients();

        // Spawn a 1-task scripted agent.
        let agent = ScriptedAgent {
            tasks: vec![ScriptedTask {
                id: "t1".into(),
                path: "src/auth.rs".into(),
                contents: "// auth\npub fn auth() {}\n".into(),
            }],
        };
        tokio::spawn(run_agent(agent, server.clone()));

        // Wire two duplex streams to the server.
        let (stream_a, srv_a) = tokio::io::duplex(65536);
        let (stream_b, srv_b) = tokio::io::duplex(65536);
        server.attach(srv_a).await;
        server.attach(srv_b).await;

        // Attach both clients (handshake happens inside attach).
        let (client_a, mut rx_a) = tokio::time::timeout(
            Duration::from_secs(5),
            SessionClient::attach(stream_a, "A".into(), seed_a),
        )
        .await
        .expect("client_a attach timed out")
        .expect("client_a attach failed");

        let (client_b, mut rx_b) = tokio::time::timeout(
            Duration::from_secs(5),
            SessionClient::attach(stream_b, "B".into(), seed_b),
        )
        .await
        .expect("client_b attach timed out")
        .expect("client_b attach failed");

        // Wait for DispatchReady on both sides.
        next_state_matching(&mut rx_a, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;
        next_state_matching(&mut rx_b, |s| {
            matches!(s.phase, WirePhase::DispatchReady { .. })
        })
        .await;

        // Both signal ready → PlanReview.
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

        // Both signal ready → Executing.
        client_a.ready().await.unwrap();
        client_b.ready().await.unwrap();

        next_state_matching(&mut rx_a, |s| matches!(s.phase, WirePhase::Executing)).await;
        next_state_matching(&mut rx_b, |s| matches!(s.phase, WirePhase::Executing)).await;

        // Wait for a gate to appear on A's stream.
        let state_with_gate = next_state_matching(&mut rx_a, |s| s.gate.is_some()).await;
        let wire_gate = state_with_gate.gate.unwrap();

        // A casts go.
        client_a
            .cast_go(&wire_gate, ReviewDepth::FullDiff)
            .await
            .unwrap();

        // B must see A's key as Sealed before B votes.
        let state_after_a = next_state_matching(&mut rx_b, |s| {
            s.gate.as_ref().map(|g| !g.keys.is_empty()).unwrap_or(false)
        })
        .await;

        let gate_b = state_after_a.gate.as_ref().unwrap();
        assert!(
            gate_b
                .keys
                .iter()
                .any(|k| k.status == VerdictStatus::Sealed),
            "A's key should be Sealed on B's view before B votes"
        );

        // B casts go.
        let wire_gate_b = state_after_a.gate.unwrap();
        client_b
            .cast_go(&wire_gate_b, ReviewDepth::FullDiff)
            .await
            .unwrap();

        // Both should see Closed with chain_verified.
        let closed_a =
            next_state_matching(&mut rx_a, |s| matches!(s.phase, WirePhase::Closed { .. })).await;
        let closed_b =
            next_state_matching(&mut rx_b, |s| matches!(s.phase, WirePhase::Closed { .. })).await;

        match &closed_a.phase {
            WirePhase::Closed { chain_verified, .. } => {
                assert!(chain_verified, "A: chain must be verified after close");
            }
            _ => panic!("expected Closed from A"),
        }
        match &closed_b.phase {
            WirePhase::Closed { chain_verified, .. } => {
                assert!(chain_verified, "B: chain must be verified after close");
            }
            _ => panic!("expected Closed from B"),
        }

        // --- Duplicate cast assertion ---
        // A tries to cast again on the same (now-closed) gate.
        // The server should reject it and send back a Rejected message on A's stream.
        client_a
            .cast_go(&wire_gate, ReviewDepth::FullDiff)
            .await
            .unwrap();

        // Drain until we get the Rejected.
        let rejected = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx_a.recv().await {
                    Some(ServerMsg::Rejected { reason }) => return reason,
                    Some(_) => {} // skip State etc.
                    None => panic!("channel closed before Rejected"),
                }
            }
        })
        .await
        .expect("timed out waiting for Rejected on duplicate cast");

        assert!(!rejected.is_empty(), "Rejected reason should not be empty");
    }

    // -----------------------------------------------------------------------
    // Smoke: SystemClock returns a plausible epoch millis value
    // -----------------------------------------------------------------------

    #[test]
    fn system_clock_is_positive() {
        let ts = SystemClock.now();
        // 2020-01-01 in millis is ~1_577_836_800_000
        assert!(
            ts.0 > 1_577_836_800_000,
            "timestamp looks too small: {}",
            ts.0
        );
    }
}
