//! End-to-end test for the `kontur mcp-bridge <port>` subcommand: it must
//! forward this process's stdin to the TCP endpoint and the endpoint's bytes
//! back to stdout, replacing the old `nc` dependency.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// The bridge round-trips bytes through a TCP echo server: what we write to the
/// child's stdin comes back on its stdout, having traversed the socket.
#[test]
fn mcp_bridge_pumps_stdio_to_tcp_and_back() {
    // Ephemeral echo server on localhost.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        loop {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sock.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_kontur"))
        .args(["mcp-bridge", &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn kontur mcp-bridge");

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    let msg = b"{\"jsonrpc\":\"2.0\"}\n";
    child_stdin.write_all(msg).unwrap();
    child_stdin.flush().unwrap();

    let mut back = vec![0u8; msg.len()];
    child_stdout.read_exact(&mut back).unwrap();
    assert_eq!(&back, msg, "bytes must round-trip through the bridge");

    // Closing stdin ends the bridge (stdin->tcp copy completes).
    drop(child_stdin);
    let _ = child.wait();
    let _ = echo.join();
}
