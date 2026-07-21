use std::sync::Arc;

use kontur_core::{Ed25519Signer, Signer};
use kontur_mcp::{GateHost, GitWorkspace, InMemoryWorkspace, SessionContext};
use kontur_net::{
    attach_tls, generate_tls, serve_agent_endpoint, ScriptedAgent, SessionConfig, SessionServer,
    WirePhase,
};
use kontur_tui::claude_agent::{agent_prompt, build_claude_command, mcp_config_json};
use kontur_tui::demo::{run, Demo};
use kontur_tui::link::{derive_seed, discover_ip, format_invite, parse_invite};
use kontur_tui::remote::run_remote;

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        // Bare `kontur` with no subcommand → zero-config host in cwd.
        None => host_cmd(&[]).await,

        Some("demo") => run(Demo::new()).await,
        Some("audit") => audit_cmd(&args[2..]),
        Some("host") => host_cmd(&args[2..]).await,
        Some("join") => join_cmd(&args[2..]).await,
        Some("help") | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        // Bare `kontur --flag ...` → zero-config host in cwd with those flags
        // (same as `kontur host --flag ...`).
        Some(flag) if flag.starts_with("--") => host_cmd(&args[1..]).await,
        Some(other) => {
            eprintln!("kontur: unknown subcommand '{other}'");
            print_usage();
            std::process::exit(1);
        }
    }
}

/// Verify a persisted audit chain: every record hash, every link, every
/// checker signature. Exit 0 with a summary on success; exit 1 naming the
/// break otherwise.
fn audit_cmd(args: &[String]) -> std::io::Result<()> {
    let Some(path) = args.first() else {
        eprintln!("usage: kontur audit <audit-file.json>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(path)?;
    let records: Vec<kontur_core::GateRecord> = serde_json::from_slice(&bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("not an audit file: {e}"),
        )
    })?;
    match kontur_core::verify_chain(&records) {
        Ok(()) => {
            let head = records
                .last()
                .map(|r| {
                    r.this_hash
                        .0
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                })
                .unwrap_or_else(|| "genesis".into());
            println!(
                "audit chain OK — {} gate{} · head sha256:{head}",
                records.len(),
                if records.len() == 1 { "" } else { "s" },
            );
            Ok(())
        }
        Err(brk) => {
            eprintln!("AUDIT CHAIN BROKEN: {brk}");
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!(
        "Usage:
  kontur                              # zero-config: host in current git repo
  kontur audit <file.json>            # verify a persisted audit chain
  kontur host [--repo <path>] [--mem] [--operator-port 7777] [--agent-port 7778]
              [--prompt \"...\"] [--claude | --demo-agent] [--seeds <hex32a,hex32b>]
              [--session <name>] [--headless]
  kontur join <kontur://ip:port/token>
  kontur join --addr host:port --seed <hex32>
  kontur demo
  kontur help

  --claude      spawn a real Claude Code agent (permission-restricted via --allowedTools /
                --disallowedTools); mutually exclusive with --demo-agent"
    );
}

// ---------------------------------------------------------------------------
// host subcommand
// ---------------------------------------------------------------------------

async fn host_cmd(args: &[String]) -> std::io::Result<()> {
    // Defaults — populated from random bytes or explicit flags.
    let mut repo: Option<String> = None;
    let mut mem = false;
    let mut operator_port: u16 = 7777;
    let mut agent_port: u16 = 7778;
    let mut prompt: Option<String> = None;
    let mut demo_agent = false;
    let mut claude_agent = false;
    let mut explicit_seeds: Option<([u8; 32], [u8; 32])> = None;
    let mut session: Option<String> = None;
    let mut headless = false;

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
                prompt = Some(require_arg(args, i, "--prompt")?);
            }
            "--demo-agent" => {
                demo_agent = true;
            }
            "--claude" => {
                claude_agent = true;
            }
            "--seeds" => {
                i += 1;
                let v = require_arg(args, i, "--seeds")?;
                let parts: Vec<&str> = v.splitn(2, ',').collect();
                if parts.len() != 2 {
                    return Err(err("--seeds: expected two comma-separated values".into()));
                }
                let sa = parse_seed_arg(parts[0].trim(), "--seeds")?;
                let sb = parse_seed_arg(parts[1].trim(), "--seeds")?;
                explicit_seeds = Some((sa, sb));
            }
            "--session" => {
                i += 1;
                session = Some(require_arg(args, i, "--session")?);
            }
            "--headless" => {
                headless = true;
            }
            other => {
                return Err(err(format!("kontur host: unknown flag '{other}'")));
            }
        }
        i += 1;
    }

    // Mutual exclusivity: --claude and --demo-agent cannot be combined.
    if claude_agent && demo_agent {
        return Err(err(
            "kontur host: --claude and --demo-agent are mutually exclusive".into(),
        ));
    }

    // Determine working repo path: explicit --repo, or cwd.
    // If neither --mem nor --repo is given, cwd must be a git repo.

    // Determine whether seeds come from explicit flags or are derived from
    // freshly-generated 16-byte secrets (v2 invite model).
    enum SeedMode {
        /// Seeds supplied explicitly via --seeds; no invite can be derived.
        Explicit([u8; 32], [u8; 32]),
        /// Seeds derived from randomly-generated secrets; invite links are available.
        Derived {
            secret_a: [u8; 16],
            secret_b: [u8; 16],
        },
    }

    let seed_mode = match explicit_seeds {
        Some((sa, sb)) => SeedMode::Explicit(sa, sb),
        None => SeedMode::Derived {
            secret_a: gen_random_16()?,
            secret_b: gen_random_16()?,
        },
    };

    let (seed_a, seed_b) = match &seed_mode {
        SeedMode::Explicit(a, b) => (*a, *b),
        SeedMode::Derived { secret_a, secret_b } => (derive_seed(secret_a), derive_seed(secret_b)),
    };

    let effective_repo = if mem {
        None
    } else if let Some(r) = repo.clone() {
        Some(r)
    } else {
        // Zero-config: use cwd. Validate it's a git repo.
        let cwd = std::env::current_dir()?;
        let git_check = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&cwd)
            .output();
        match git_check {
            Ok(out) if out.status.success() => Some(cwd.to_string_lossy().into_owned()),
            _ => {
                eprintln!(
                    "error: current directory is not a git repository.\n\
                     hint: run `git init` first, or use `kontur host --mem` for an in-memory session."
                );
                std::process::exit(1);
            }
        }
    };

    // Session name: explicit, or auto-generated s-<6hex>.
    let session_name = match session {
        Some(s) => s,
        None => {
            let mut b = [0u8; 3];
            gen_random_bytes(&mut b)?;
            format!("s-{:02x}{:02x}{:02x}", b[0], b[1], b[2])
        }
    };

    // The prompt starts blank unless --prompt pre-seeds it: it is the
    // operators' instruction, composed in-console at the dispatch gate.
    // The server refuses dispatch while it is empty.
    let effective_prompt = prompt.unwrap_or_default();

    // Derive operators from seeds.
    let op_a = Ed25519Signer::from_seed(seed_a).operator_id();
    let op_b = Ed25519Signer::from_seed(seed_b).operator_id();

    // Build session context + workspace.
    let ctx = SessionContext::new(
        &effective_prompt,
        op_a,
        "agent-01",
        "external",
        "1.0",
        vec![op_a, op_b],
    );

    let host: Arc<GateHost> = if mem || effective_repo.is_none() {
        let ws = Arc::new(InMemoryWorkspace::new());
        Arc::new(GateHost::new(ctx, ws))
    } else {
        let repo_path = std::path::PathBuf::from(effective_repo.as_deref().unwrap());
        let ws = GitWorkspace::create(repo_path, &session_name)
            .map_err(|e| err(format!("git workspace: {e}")))?;
        Arc::new(GateHost::new(ctx, Arc::new(ws)))
    };

    // Session server.
    let cfg = SessionConfig {
        prompt: effective_prompt.clone(),
        plan: vec!["external agent tasks".into()],
        seats: [("HOST".into(), op_a), ("OPERATOR".into(), op_b)],
    };
    let server = SessionServer::new(host.clone(), cfg);

    // Bind operator listener.
    let op_listener = tokio::net::TcpListener::bind(("0.0.0.0", operator_port)).await?;
    let op_addr = op_listener.local_addr()?;

    // Bind agent listener (localhost only — agent endpoint stays plaintext;
    // CC connects via nc on 127.0.0.1).
    let agent_listener = tokio::net::TcpListener::bind(("127.0.0.1", agent_port)).await?;
    let agent_addr = agent_listener.local_addr()?;

    // Generate per-session TLS for the operator wire.
    let session_tls = generate_tls();
    let fp16 = session_tls.fingerprint16();
    let acceptor = session_tls.acceptor.clone();

    // Spawn operator accept loop (TLS).
    {
        let server_clone = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = op_listener.accept().await else {
                    break;
                };
                attach_tls(&server_clone, &acceptor, stream).await;
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

    // Optionally spawn the real Claude Code agent.
    // Build the MCP config file path now (session-scoped temp dir).
    let claude_log_path = if claude_agent {
        let tmp_dir = std::env::temp_dir().join(format!("kontur-{session_name}"));
        std::fs::create_dir_all(&tmp_dir)?;

        let mcp_config_path = tmp_dir.join("kontur-mcp.json");
        let log_path = tmp_dir.join("claude-agent.log");

        // Write the MCP bridge config.
        let config_json = mcp_config_json(agent_port);
        std::fs::write(&mcp_config_path, &config_json)?;

        let mcp_config_str = mcp_config_path
            .to_str()
            .ok_or_else(|| err("session temp path is not valid UTF-8".into()))?
            .to_owned();

        let log_path_clone = log_path.clone();
        let server_clone = server.clone();
        tokio::spawn(async move {
            // Wait until the dispatch gate clears and the phase reaches PlanReview.
            // We wait here (not at CLI time) so that any in-console prompt edits
            // made during DispatchReady are captured by session_prompt() below.
            let mut state_rx = server_clone.state_rx();
            loop {
                {
                    let state = state_rx.borrow_and_update().clone();
                    if matches!(state.phase, WirePhase::PlanReview { .. })
                        || matches!(state.phase, WirePhase::Executing)
                        || matches!(state.phase, WirePhase::Closed { .. })
                    {
                        break;
                    }
                }
                if state_rx.changed().await.is_err() {
                    break;
                }
            }

            // Fetch the (potentially edited) prompt after dispatch cleared.
            let dispatched_prompt = server_clone.session_prompt().await;
            let full_prompt = agent_prompt(&dispatched_prompt);
            let cmd = build_claude_command(&mcp_config_str, &full_prompt);

            // Open the log file for stdout/stderr.
            let log_file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path_clone)
            {
                Ok(f) => f,
                Err(e) => {
                    server_clone
                        .agent_log(format!("claude agent: failed to open log: {e}"))
                        .await;
                    return;
                }
            };

            // Spawn the claude child. Use tokio::process for async wait.
            let stderr_file = match log_file.try_clone() {
                Ok(f) => f,
                Err(e) => {
                    server_clone
                        .agent_log(format!("claude agent: failed to clone log fd: {e}"))
                        .await;
                    return;
                }
            };

            use std::process::Stdio;
            use tokio::process::Command as TokioCommand;

            let mut child = match TokioCommand::new(&cmd.program)
                .args(&cmd.args)
                .stdout(Stdio::from(log_file))
                .stderr(Stdio::from(stderr_file))
                // Backstop: if this task is dropped (e.g. the process exits
                // before the select! below can send SIGKILL), the OS child is
                // still killed. tokio::process::Child does NOT kill on drop by
                // default, so we opt-in explicitly.
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => {
                    server_clone
                        .agent_log(format!(
                            "claude agent launched (log: {})",
                            log_path_clone.display()
                        ))
                        .await;
                    c
                }
                Err(e) => {
                    let msg = if e.kind() == std::io::ErrorKind::NotFound {
                        format!(
                            "claude agent: 'claude' not found on PATH — install Claude Code first. Error: {e}"
                        )
                    } else {
                        format!("claude agent: failed to spawn: {e}")
                    };
                    server_clone.agent_log(msg).await;
                    return;
                }
            };

            // Watch for session close (abandoned or normal) so we can kill the
            // child promptly. select! races the child's own exit against a
            // WirePhase::Closed observation. Whichever fires first wins.
            let mut close_rx = server_clone.state_rx();
            tokio::select! {
                // Branch 1: child exited on its own.
                wait_result = child.wait() => {
                    match wait_result {
                        Ok(status) if status.success() => {
                            server_clone.agent_done().await;
                        }
                        Ok(status) => {
                            let code = status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into());
                            server_clone
                                .agent_log(format!(
                                    "claude agent exited with status {code} — see {}",
                                    log_path_clone.display()
                                ))
                                .await;
                            use kontur_net::WireFleetCard;
                            server_clone
                                .agent_status(WireFleetCard {
                                    id: "claude-01".into(),
                                    status: format!("FAILED — see {}", log_path_clone.display()),
                                    tokens: 0,
                                    needs_signoff: false,
                                })
                                .await;
                        }
                        Err(e) => {
                            server_clone
                                .agent_log(format!("claude agent: wait error: {e}"))
                                .await;
                        }
                    }
                }
                // Branch 2: session closed (abandoned or normal close) before
                // the child exited — send SIGKILL so the child does not linger.
                _ = async {
                    loop {
                        {
                            let state = close_rx.borrow_and_update().clone();
                            if matches!(state.phase, WirePhase::Closed { .. }) {
                                break;
                            }
                        }
                        if close_rx.changed().await.is_err() {
                            break;
                        }
                    }
                } => {
                    // Session closed — kill the child and wait for it to reap.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    server_clone
                        .agent_log("claude agent stopped (session closed)".into())
                        .await;
                }
            }
        });

        Some(log_path)
    } else {
        None
    };

    // Discover IP and format invite link (includes TLS fingerprint).
    let lan_ip = kontur_tui::link::discover_lan_ip();
    let public_ip = kontur_tui::link::discover_public_ip();
    let ip = lan_ip.clone().unwrap_or_else(discover_ip);

    // Invite links are only available when seeds are derived (not explicit).
    let (invite_link, remote_link) = match &seed_mode {
        SeedMode::Explicit(_, _) => (None, None),
        SeedMode::Derived { secret_b, .. } => {
            let link = format_invite(&ip, op_addr.port(), secret_b, &fp16);
            let remote = match &public_ip {
                Some(pubip) if Some(pubip) != lan_ip.as_ref() => {
                    Some(format_invite(pubip, op_addr.port(), secret_b, &fp16))
                }
                _ => None,
            };
            (Some(link), remote)
        }
    };

    // Print session info and invite block.
    println!("kontur host running  ·  session {session_name}");
    println!("  operator port : {op_addr}");
    println!("  agent port    : {agent_addr}");
    println!();
    if let Some(ref link) = invite_link {
        println!("  invite your operator — send them this (over a private channel; the link IS their key):");
        println!("    kontur join {link}");
        if let Some(remote) = &remote_link {
            println!();
            println!(
                "  remote operator (off your network)? forward port {} on your router, then send:",
                op_addr.port()
            );
            println!("    kontur join {remote}");
        }
    } else {
        println!("  explicit seeds: share connection details manually; join with --addr/--seed");
    }
    println!();
    if let Some(ref log_path) = claude_log_path {
        println!(
            "  spawning claude code as the agent (log: {})",
            log_path.display()
        );
        println!("  the agent will launch once both seats approve the dispatch gate.");
    } else {
        println!("  attach a real Claude Code as the agent (--claude flag, or manually):");
        println!("  primary path:");
        println!("    kontur host --claude --prompt \"<your task>\"");
        println!();
        println!("  alternative (manual bridge):");
        println!("    1. save as kontur-mcp.json:");
        println!("       {{\"mcpServers\":{{\"kontur\":{{\"command\":\"nc\",\"args\":[\"127.0.0.1\",\"{}\"]}}}}}}",  agent_addr.port());
        println!("    2. run: claude --mcp-config kontur-mcp.json \\");
        println!("         --allowedTools \"mcp__kontur__*\" \\");
        println!("         --disallowedTools Write Edit MultiEdit NotebookEdit Bash \\");
        println!("         --permission-mode default \\");
        println!("         -p \"<your protocol prompt>\"");
    }
    println!();

    if headless {
        println!("Press Ctrl-C to stop.");
        tokio::signal::ctrl_c().await?;
        println!("\nkontur host shutting down.");
    } else {
        let host_addr = format!("127.0.0.1:{}", op_addr.port());
        let links = match &seed_mode {
            SeedMode::Explicit(_, _) => None,
            SeedMode::Derived { secret_b, .. } => {
                let lan_cmd = lan_ip
                    .as_ref()
                    .map(|lip| {
                        format!(
                            "kontur join {}",
                            format_invite(lip, op_addr.port(), secret_b, &fp16)
                        )
                    })
                    .or_else(|| invite_link.as_ref().map(|l| format!("kontur join {l}")));
                let wan_cmd = remote_link.as_ref().map(|r| format!("kontur join {r}"));
                Some(kontur_tui::link::InviteLinks {
                    lan: lan_cmd,
                    wan: wan_cmd,
                    port: op_addr.port(),
                })
            }
        };
        run_remote(&host_addr, "HOST".into(), seed_a, links, Some(fp16)).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// join subcommand
// ---------------------------------------------------------------------------

async fn join_cmd(args: &[String]) -> std::io::Result<()> {
    // If the first arg looks like a kontur:// link, parse it directly.
    if let Some(first) = args.first() {
        if first.starts_with("kontur://") {
            let (addr, secret16, fp16) =
                parse_invite(first).map_err(|e| err(format!("invalid invite link: {e}")))?;
            let seed = derive_seed(&secret16);
            return run_remote(&addr, "OPERATOR".into(), seed, None, Some(fp16)).await;
        }
    }

    // Legacy form: kontur join --addr X --seed N (no TLS — deprecated path)
    let mut addr: Option<String> = None;
    let mut seed_str: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" => {
                i += 1;
                addr = Some(require_arg(args, i, "--addr")?);
            }
            "--seed" => {
                i += 1;
                seed_str = Some(require_arg(args, i, "--seed")?);
            }
            other => {
                return Err(err(format!("kontur join: unknown flag '{other}'")));
            }
        }
        i += 1;
    }

    let addr = addr.ok_or_else(|| err("kontur join: --addr is required".into()))?;
    let seed_val_str = seed_str.ok_or_else(|| err("kontur join: --seed is required".into()))?;
    let seed = parse_seed_arg(&seed_val_str, "--seed")?;

    // Legacy --addr/--seed path: no fingerprint → plain TCP (deprecated)
    run_remote(&addr, "OPERATOR".into(), seed, None, None).await
}

// ---------------------------------------------------------------------------
// Seed helpers
// ---------------------------------------------------------------------------

/// Parse a seed argument: accepts either a small integer (1..=255, for
/// backward-compat with old --seeds 1,2 form) OR a 64-char hex string (full
/// 32-byte seed).
fn parse_seed_arg(s: &str, flag: &str) -> std::io::Result<[u8; 32]> {
    // Try 64-char hex first.
    if s.len() == 64 {
        if let Some(seed) = hex_to_seed_opt(s) {
            return Ok(seed);
        }
    }
    // Try small integer (backward-compat).
    if let Ok(n) = s.parse::<u8>() {
        return Ok([n; 32]);
    }
    Err(err(format!(
        "{flag}: invalid seed '{s}': expected a small integer (1-255) or 64 hex chars"
    )))
}

fn hex_to_seed_opt(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Generate 16 random bytes using getrandom (for v2 invite secrets).
fn gen_random_16() -> std::io::Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    gen_random_bytes(&mut buf)?;
    Ok(buf)
}

fn gen_random_bytes(buf: &mut [u8]) -> std::io::Result<()> {
    getrandom::getrandom(buf).map_err(|e| err(format!("getrandom: {e}")))
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
