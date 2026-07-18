use std::sync::Arc;

use kontur_core::{Ed25519Signer, Signer};
use kontur_mcp::{GateHost, GitWorkspace, InMemoryWorkspace, SessionContext};
use kontur_net::{ScriptedAgent, SessionConfig, SessionServer, serve_agent_endpoint};
use kontur_tui::demo::{run, Demo};
use kontur_tui::remote::run_remote;

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        None | Some("demo") => {
            // Default: local self-contained demo.
            run(Demo::new()).await
        }
        Some("host") => host_cmd(&args[2..]).await,
        Some("join") => join_cmd(&args[2..]).await,
        Some("help") | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("kontur: unknown subcommand '{other}'");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!(
        "Usage:
  kontur demo
  kontur host --repo <path> [--mem] [--operator-port 7777] [--agent-port 7778]
              [--prompt \"...\"] [--demo-agent] [--seeds 1,2] [--session <name>]
  kontur join --addr host:port --seat A|B --seed <n>
  kontur help"
    );
}

// ---------------------------------------------------------------------------
// host subcommand
// ---------------------------------------------------------------------------

async fn host_cmd(args: &[String]) -> std::io::Result<()> {
    // Defaults
    let mut repo: Option<String> = None;
    let mut mem = false;
    let mut operator_port: u16 = 7777;
    let mut agent_port: u16 = 7778;
    let mut prompt = String::from("kontur session");
    let mut demo_agent = false;
    let mut seeds: [u8; 2] = [1, 2];
    let mut session = String::from("s1");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                i += 1;
                repo = Some(require_arg(args, i, "--repo")?);
            }
            "--mem" => {
                mem = true;
            }
            "--operator-port" => {
                i += 1;
                let v = require_arg(args, i, "--operator-port")?;
                operator_port = v
                    .parse()
                    .map_err(|_| err(format!("--operator-port: not a valid port: {v}")))?;
            }
            "--agent-port" => {
                i += 1;
                let v = require_arg(args, i, "--agent-port")?;
                agent_port = v
                    .parse()
                    .map_err(|_| err(format!("--agent-port: not a valid port: {v}")))?;
            }
            "--prompt" => {
                i += 1;
                prompt = require_arg(args, i, "--prompt")?;
            }
            "--demo-agent" => {
                demo_agent = true;
            }
            "--seeds" => {
                i += 1;
                let v = require_arg(args, i, "--seeds")?;
                let parts: Vec<&str> = v.split(',').collect();
                if parts.len() != 2 {
                    return Err(err("--seeds: expected two comma-separated integers, e.g. 1,2".into()));
                }
                seeds[0] = parts[0]
                    .trim()
                    .parse()
                    .map_err(|_| err(format!("--seeds: invalid seed '{}'", parts[0])))?;
                seeds[1] = parts[1]
                    .trim()
                    .parse()
                    .map_err(|_| err(format!("--seeds: invalid seed '{}'", parts[1])))?;
            }
            "--session" => {
                i += 1;
                session = require_arg(args, i, "--session")?;
            }
            other => {
                return Err(err(format!("kontur host: unknown flag '{other}'")));
            }
        }
        i += 1;
    }

    // Derive operators from seeds.
    let op_a = Ed25519Signer::from_seed([seeds[0]; 32]).operator_id();
    let op_b = Ed25519Signer::from_seed([seeds[1]; 32]).operator_id();

    // Build session context + workspace.
    let ctx = SessionContext::new(
        &prompt,
        op_a,
        "agent-01",
        "external",
        "1.0",
        vec![op_a, op_b],
    );

    let host: Arc<GateHost> = if mem || repo.is_none() {
        let ws = Arc::new(InMemoryWorkspace::new());
        Arc::new(GateHost::new(ctx, ws))
    } else {
        let repo_path = std::path::PathBuf::from(repo.as_deref().unwrap());
        let ws = GitWorkspace::create(repo_path, &session)
            .map_err(|e| err(format!("git workspace: {e}")))?;
        Arc::new(GateHost::new(ctx, Arc::new(ws)))
    };

    // Session server.
    let cfg = SessionConfig {
        prompt: prompt.clone(),
        plan: vec!["external agent tasks".into()],
        seats: [("A".into(), op_a), ("B".into(), op_b)],
    };
    let server = SessionServer::new(host.clone(), cfg);

    // Bind operator listener.
    let op_listener = tokio::net::TcpListener::bind(("0.0.0.0", operator_port)).await?;
    let op_addr = op_listener.local_addr()?;

    // Bind agent listener.
    let agent_listener = tokio::net::TcpListener::bind(("0.0.0.0", agent_port)).await?;
    let agent_addr = agent_listener.local_addr()?;

    // Spawn operator accept loop.
    {
        let server_clone = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = op_listener.accept().await else { break };
                server_clone.attach(stream).await;
            }
        });
    }

    // Spawn agent MCP endpoint.
    {
        let host_clone = host.clone();
        tokio::spawn(serve_agent_endpoint(agent_listener, host_clone));
    }

    // Optionally spawn the demo scripted agent.
    if demo_agent {
        let server_clone = server.clone();
        tokio::spawn(async move {
            ScriptedAgent::demo().run(server_clone).await;
        });
    }

    // Print join hints.
    println!("kontur host running");
    println!("  operator port : {op_addr}");
    println!("  agent port    : {agent_addr}");
    println!();
    println!("  kontur join --addr 127.0.0.1:{} --seat A --seed {}", op_addr.port(), seeds[0]);
    println!("  kontur join --addr 127.0.0.1:{} --seat B --seed {}", op_addr.port(), seeds[1]);
    println!();
    println!("  MCP agent: {{\"command\": \"nc\", \"args\": [\"localhost\", \"{}\"]}}",  agent_addr.port());
    println!();
    println!("Press Ctrl-C to stop.");

    // Park until Ctrl-C.
    tokio::signal::ctrl_c().await?;
    println!("\nkontur host shutting down.");
    Ok(())
}

// ---------------------------------------------------------------------------
// join subcommand
// ---------------------------------------------------------------------------

async fn join_cmd(args: &[String]) -> std::io::Result<()> {
    let mut addr: Option<String> = None;
    let mut seat: Option<String> = None;
    let mut seed_val: u8 = 1;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" => {
                i += 1;
                addr = Some(require_arg(args, i, "--addr")?);
            }
            "--seat" => {
                i += 1;
                seat = Some(require_arg(args, i, "--seat")?);
            }
            "--seed" => {
                i += 1;
                let v = require_arg(args, i, "--seed")?;
                seed_val = v
                    .parse()
                    .map_err(|_| err(format!("--seed: not a valid integer: {v}")))?;
            }
            other => {
                return Err(err(format!("kontur join: unknown flag '{other}'")));
            }
        }
        i += 1;
    }

    let addr = addr.ok_or_else(|| err("kontur join: --addr is required".into()))?;
    let seat = seat.ok_or_else(|| err("kontur join: --seat is required".into()))?;

    let seed_bytes = [seed_val; 32];
    run_remote(&addr, seat, seed_bytes).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_arg(args: &[String], i: usize, flag: &str) -> std::io::Result<String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| err(format!("{flag}: expected a value")))
}

fn err(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}
