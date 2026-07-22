use kontur_core::{HoldState, TaskId};

use crate::protocol::WireFleetCard;
use crate::server::{ScriptedAgent, ScriptedTask, SessionServer};

pub async fn run_agent(agent: ScriptedAgent, server: SessionServer) {
    // Wait until plan is approved
    let mut plan_rx = server.plan_approved_rx();
    loop {
        if *plan_rx.borrow_and_update() {
            break;
        }
        if plan_rx.changed().await.is_err() {
            return;
        }
    }

    let mut tasks: Vec<ScriptedTask> = agent.tasks;

    for (i, task) in tasks.iter_mut().enumerate() {
        // Signal working
        server
            .agent_status(WireFleetCard {
                id: task.id.clone(),
                status: "working".into(),
                needs_signoff: false,
            })
            .await;

        // Record the write
        server
            .host()
            .record_write(
                "agent-01",
                &TaskId(task.id.clone()),
                &task.path,
                task.contents.as_bytes(),
            )
            .await
            .unwrap();

        // Open a gate
        let (gate_id, mut rx) = server
            .host()
            .begin_task_gate("agent-01", TaskId(task.id.clone()), 100 * (i as u64 + 1))
            .await
            .unwrap();

        // Signal gate is open so the server pushes an updated WireState with the gate
        server
            .agent_status(WireFleetCard {
                id: task.id.clone(),
                status: "awaiting-signoff".into(),
                needs_signoff: true,
            })
            .await;

        // Wait for resolution
        let resolved_state = await_gate(&mut rx).await;

        if resolved_state == HoldState::Satisfied {
            server
                .agent_status(WireFleetCard {
                    id: task.id.clone(),
                    status: "done".into(),
                    needs_signoff: false,
                })
                .await;
            continue;
        }

        // Blocked — rework loop (cap at 3 attempts)
        let mut current_gate_id = gate_id;
        let mut attempts = 0usize;

        loop {
            if resolved_state == HoldState::Satisfied {
                server
                    .agent_status(WireFleetCard {
                        id: task.id.clone(),
                        status: "done".into(),
                        needs_signoff: false,
                    })
                    .await;
                break;
            }

            if attempts >= 3 {
                server
                    .agent_log(format!("rework cap reached for {}", task.id))
                    .await;
                break;
            }
            attempts += 1;

            // Extract remedy
            let steer = match server.host().gate_outcome(&current_gate_id).await {
                Some(final_gate) => match final_gate.remedy {
                    Some(kontur_core::Remedy::Steer(s)) => s,
                    Some(kontur_core::Remedy::HandEdit(_)) => "hand-edit".into(),
                    None => "rework".into(),
                },
                None => "rework".into(),
            };

            // Apply fix
            let fixed = format!("{}\n// fix: {steer}\n", task.contents);
            task.contents = fixed;

            // Re-propose
            server
                .host()
                .record_write(
                    "agent-01",
                    &TaskId(task.id.clone()),
                    &task.path,
                    task.contents.as_bytes(),
                )
                .await
                .unwrap();

            let (new_gate_id, mut new_rx) = server
                .host()
                .begin_task_gate("agent-01", TaskId(task.id.clone()), 100 * (i as u64 + 1))
                .await
                .unwrap();

            current_gate_id = new_gate_id;

            // Signal the new gate so the server pushes an updated WireState
            server
                .agent_status(WireFleetCard {
                    id: task.id.clone(),
                    status: "awaiting-signoff".into(),
                    needs_signoff: true,
                })
                .await;

            let new_state = await_gate(&mut new_rx).await;

            if new_state == HoldState::Satisfied {
                server
                    .agent_status(WireFleetCard {
                        id: task.id.clone(),
                        status: "done".into(),
                        needs_signoff: false,
                    })
                    .await;
                break;
            }

            // Still blocked — loop again
        }
    }

    // All tasks done
    server
        .agent_status(WireFleetCard {
            id: "agent".into(),
            status: "idle".into(),
            needs_signoff: false,
        })
        .await;

    server.agent_done().await;
}

async fn await_gate(rx: &mut tokio::sync::watch::Receiver<HoldState>) -> HoldState {
    loop {
        let s = *rx.borrow_and_update();
        if matches!(s, HoldState::Satisfied | HoldState::Blocked) {
            return s;
        }
        if rx.changed().await.is_err() {
            return HoldState::Open;
        }
    }
}

impl ScriptedAgent {
    pub async fn run(self, server: SessionServer) {
        run_agent(self, server).await
    }
}
