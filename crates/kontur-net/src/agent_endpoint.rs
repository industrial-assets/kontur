use std::sync::Arc;

use kontur_mcp::GateHost;

/// Accept MCP agent connections on `listener`. Each accepted TCP stream is
/// handed to a fresh `KonturServer` instance served by rmcp over that stream.
/// Returns when the listener errors (typically on shutdown).
pub async fn serve_agent_endpoint(listener: tokio::net::TcpListener, host: Arc<GateHost>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { break };
        let server = kontur_mcp::KonturServer::new(host.clone());
        tokio::spawn(async move {
            if let Ok(running) = rmcp::serve_server(server, stream).await {
                let _ = running.waiting().await;
            }
        });
    }
}
