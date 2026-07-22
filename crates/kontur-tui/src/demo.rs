use std::sync::Arc;
use std::time::Duration;

use kontur_core::{
    CastVerdict, Ed25519Signer, FixedClock, GateId, Hash, ReviewDepth, Signer, TaskId, Verdict,
};
use kontur_mcp::{GateHost, InMemoryWorkspace, SessionContext, Workspace};

use crate::app::{poll_action, TerminalGuard};
use crate::fleet::MockFleet;
use crate::input::Action;
use crate::render::render;
use crate::view::{Banner, LogLine, Role, Station};
use crate::viewmodel::build_session_view;

/// A self-contained demo session: real GateHost + in-memory workspace + a
/// scripted second operator. Station A is the live keyboard operator; B is
/// scripted (this is a dev/demo console — not the production two-human seat).
pub struct Demo {
    host: Arc<GateHost>,
    workspace: Arc<InMemoryWorkspace>,
    signer_a: Ed25519Signer,
    signer_b: Ed25519Signer,
}

impl Demo {
    pub fn new() -> Self {
        let signer_a = Ed25519Signer::from_seed([1; 32]);
        let signer_b = Ed25519Signer::from_seed([2; 32]);
        let (op_a, op_b) = (signer_a.operator_id(), signer_b.operator_id());
        let workspace = Arc::new(InMemoryWorkspace::new());
        let ctx = SessionContext::new(
            "refactor the session guard to the new token store",
            op_a,
            "agent-03",
            "claude-opus-4-8",
            "1.0",
            vec![op_a, op_b],
        );
        let host = Arc::new(GateHost::new(ctx, workspace.clone()));
        Demo {
            host,
            workspace,
            signer_a,
            signer_b,
        }
    }

    pub fn host(&self) -> &Arc<GateHost> {
        &self.host
    }

    pub fn stations(&self) -> [Station; 2] {
        [
            Station {
                label: "Operator A [Host]".into(),
                role: Role::Host,
                activity: "reviewing".into(),
                operator: self.signer_a.operator_id(),
                afk: false,
            },
            Station {
                label: "Operator B".into(),
                role: Role::Operator,
                activity: "reviewing".into(),
                operator: self.signer_b.operator_id(),
                afk: false,
            },
        ]
    }

    pub fn banner(&self) -> Banner {
        Banner {
            session: "4417".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    /// Script an agent producing a change and parking it at a gate.
    pub async fn open_demo_gate(&self) -> (GateId, Hash) {
        let task = TaskId("t1".into());
        self.workspace
            .apply_write(
                &task,
                "auth/session.rs",
                b"// guarded token store\nfn guard() {}\n",
            )
            .unwrap();
        let (gid, _rx) = self
            .host
            .begin_task_gate("agent-01", task, 6400)
            .await
            .unwrap();
        let dh = self.host.pending_gates().await[0].diff_hash;
        (gid, dh)
    }

    /// Station A's signed go (the live operator's key).
    pub fn go_a(&self, gid: &GateId, dh: Hash) -> CastVerdict {
        CastVerdict::create(
            &self.signer_a,
            &FixedClock(1000),
            gid,
            dh,
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }

    /// The scripted second operator's go.
    pub fn go_b(&self, gid: &GateId, dh: Hash) -> CastVerdict {
        CastVerdict::create(
            &self.signer_b,
            &FixedClock(1001),
            gid,
            dh,
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }
}

impl Default for Demo {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the interactive demo console. Station A drives from the keyboard; when A
/// casts `go`, the scripted second key follows and the gate resolves.
pub async fn run(demo: Demo) -> std::io::Result<()> {
    let fleet = MockFleet::demo();
    let (gid, dh) = demo.open_demo_gate().await;
    let mut log: Vec<LogLine> = vec![LogLine {
        time: "12:10".into(),
        who: "agent-03".into(),
        text: "parked change at gate-001".into(),
    }];
    let mut closed = false;

    let (_guard, mut terminal) = TerminalGuard::enter()?;

    // Boot card: identity, version, provenance — then the console takes over.
    terminal.draw(|f| crate::boot::render_boot(f, env!("CARGO_PKG_VERSION")))?;
    tokio::time::sleep(std::time::Duration::from_millis(crate::boot::BOOT_HOLD_MS)).await;

    loop {
        let view = build_session_view(
            demo.host(),
            &fleet,
            demo.stations(),
            demo.banner(),
            log.clone(),
            closed,
        )
        .await;
        terminal.draw(|f| render(f, &view, 0, 0, 0, &std::cell::Cell::new(0)))?;
        if closed {
            // Draw the final frame, then wait for a quit key.
            if let Some(Action::Quit) =
                poll_action(Duration::from_millis(200), false, false, false)?
            {
                break;
            }
            continue;
        }
        match poll_action(Duration::from_millis(200), false, false, false)? {
            Some(Action::Quit) => break,
            Some(Action::Go) => {
                let _ = demo.host().submit_verdict(&gid, demo.go_a(&gid, dh)).await;
                log.push(LogLine {
                    time: "12:11".into(),
                    who: "you".into(),
                    text: "go gate-001 · key sealed".into(),
                });
                // Scripted second key follows; only close on a real Satisfied resolution.
                match demo.host().submit_verdict(&gid, demo.go_b(&gid, dh)).await {
                    Ok(progress) if progress.state == kontur_core::HoldState::Satisfied => {
                        log.push(LogLine {
                            time: "12:11".into(),
                            who: "j.reed".into(),
                            text: "go gate-001 · unanimous".into(),
                        });
                        closed = true;
                    }
                    Ok(_) => {
                        log.push(LogLine {
                            time: "12:11".into(),
                            who: "kontur".into(),
                            text: "gate not yet resolved".into(),
                        });
                    }
                    Err(_) => {
                        log.push(LogLine {
                            time: "12:11".into(),
                            who: "kontur".into(),
                            text: "second key rejected".into(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    TerminalGuard::restore();
    Ok(())
}
