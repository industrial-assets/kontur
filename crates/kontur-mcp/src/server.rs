use std::sync::Arc;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use kontur_core::{HoldState, Remedy, TaskId};

use crate::gatehost::{ClarifyDecision, ClarifyQuestion, GateHost, PlanDecision};

/// The rmcp server exposing the agent-facing gated tools over a `GateHost`.
#[derive(Clone)]
pub struct KonturServer {
    host: Arc<GateHost>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct WriteFileInput {
    pub task_id: String,
    pub path: String,
    pub contents: String,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct OkOutput {
    pub ok: bool,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct RunCommandInput {
    pub task_id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct CommandOut {
    pub stdout: String,
    pub exit_code: i32,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ProposeInput {
    pub task_id: String,
    #[serde(default)]
    pub tokens: u64,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ProposeOutput {
    pub accepted: bool,
    pub reviewed_by: Vec<String>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ProposePlanInput {
    pub tasks: Vec<String>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ProposePlanOutput {
    pub approved: bool,
    /// The APPROVED task list, which operators may have edited, deleted from,
    /// or reordered from the original proposal. Execute exactly this list.
    pub tasks: Vec<String>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ClarifyQuestionInput {
    /// The question to put to the operators.
    pub prompt: String,
    /// The multiple-choice options. The console always adds a final
    /// "provide your own answer" free-text option; do not include it here.
    pub options: Vec<String>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AskClarificationInput {
    pub questions: Vec<ClarifyQuestionInput>,
}

#[derive(Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AskClarificationOutput {
    /// The operators' accepted answers, one entry per question in order. An
    /// entry has two strings only when the operators chose "accept both".
    pub answers: Vec<Vec<String>>,
}

impl KonturServer {
    pub fn new(host: Arc<GateHost>) -> Self {
        KonturServer { host }
    }
}

#[tool_router(server_handler)]
impl KonturServer {
    #[tool(
        name = "write_file",
        description = "Write a file in the agent's worktree (recorded, not gated)."
    )]
    async fn write_file(
        &self,
        Parameters(WriteFileInput {
            task_id,
            path,
            contents,
        }): Parameters<WriteFileInput>,
    ) -> Result<Json<OkOutput>, ErrorData> {
        self.host
            .record_write(&TaskId(task_id), &path, contents.as_bytes())
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(Json(OkOutput { ok: true }))
    }

    #[tool(
        name = "run_command",
        description = "Run a command in the agent's worktree (recorded, not gated)."
    )]
    async fn run_command(
        &self,
        Parameters(RunCommandInput {
            task_id,
            command,
            cwd,
        }): Parameters<RunCommandInput>,
    ) -> Result<Json<CommandOut>, ErrorData> {
        let out = self
            .host
            .run_command(&TaskId(task_id), &command, &cwd)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(Json(CommandOut {
            stdout: out.stdout,
            exit_code: out.exit_code,
        }))
    }

    #[tool(
        name = "propose_plan",
        description = "Submit the agent's task plan for operator approval; blocks until both operators approve."
    )]
    async fn propose_plan(
        &self,
        Parameters(ProposePlanInput { tasks }): Parameters<ProposePlanInput>,
    ) -> Result<Json<ProposePlanOutput>, ErrorData> {
        let mut rx = self
            .host
            .propose_plan(tasks)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Await a decision. borrow_and_update reads the current value and marks
        // it seen; loop until terminal. Approved breaks; Steered returns an
        // error carrying the remedy so the agent revises and re-proposes. A
        // closed channel means the session closed.
        loop {
            match rx.borrow_and_update().clone() {
                PlanDecision::Pending => {}
                PlanDecision::Approved => break,
                PlanDecision::Steered(s) => {
                    return Err(ErrorData::invalid_request(
                        format!(
                            "plan rejected: {s} — revise the task list and call propose_plan again"
                        ),
                        None,
                    ));
                }
            }
            if rx.changed().await.is_err() {
                return Err(ErrorData::internal_error("session closed", None));
            }
        }

        // Return the APPROVED task list — operators may have edited it.
        let final_tasks = self.host.proposed_plan().await.unwrap_or_default();
        Ok(Json(ProposePlanOutput {
            approved: true,
            tasks: final_tasks,
        }))
    }

    #[tool(
        name = "ask_clarification",
        description = "Ask the operators to resolve genuine ambiguity in the prompt BEFORE planning. Only call this when the prompt is truly ambiguous and you would otherwise have to assume; never ask gratuitously. Each question is multiple-choice (the console adds a 'provide your own answer' option). Blocks until both operators answer and agree; returns their accepted answers."
    )]
    async fn ask_clarification(
        &self,
        Parameters(AskClarificationInput { questions }): Parameters<AskClarificationInput>,
    ) -> Result<Json<AskClarificationOutput>, ErrorData> {
        if questions.is_empty() {
            return Err(ErrorData::invalid_request(
                "ask_clarification needs at least one question; if the prompt is clear, call propose_plan instead",
                None,
            ));
        }
        let questions: Vec<ClarifyQuestion> = questions
            .into_iter()
            .map(|q| ClarifyQuestion {
                prompt: q.prompt,
                options: q.options,
            })
            .collect();
        let mut rx = self
            .host
            .ask_clarification(questions)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        loop {
            match rx.borrow_and_update().clone() {
                ClarifyDecision::Pending => {}
                ClarifyDecision::Answered(answers) => {
                    return Ok(Json(AskClarificationOutput { answers }));
                }
            }
            if rx.changed().await.is_err() {
                return Err(ErrorData::internal_error("session closed", None));
            }
        }
    }

    #[tool(
        name = "propose_task_complete",
        description = "Submit the completed task for four-eyes review; blocks until both operators sign off."
    )]
    async fn propose_task_complete(
        &self,
        Parameters(ProposeInput { task_id, tokens }): Parameters<ProposeInput>,
    ) -> Result<Json<ProposeOutput>, ErrorData> {
        let task_id = TaskId(task_id);
        let (gate_id, mut rx) = self
            .host
            .begin_task_gate(task_id, tokens)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Await resolution. `borrow_and_update` reads the latest state; loop
        // until terminal. A closed channel means the session is shutting down.
        loop {
            let state = *rx.borrow_and_update();
            if matches!(state, HoldState::Satisfied | HoldState::Blocked) {
                break;
            }
            if rx.changed().await.is_err() {
                return Err(ErrorData::internal_error(
                    "session closed before gate resolved",
                    None,
                ));
            }
        }

        let final_ = self
            .host
            .gate_outcome(&gate_id)
            .await
            .ok_or_else(|| ErrorData::internal_error("gate disappeared", None))?;

        match final_.state {
            HoldState::Satisfied => Ok(Json(ProposeOutput {
                accepted: true,
                reviewed_by: final_.reviewed_by.iter().map(|o| hex32(&o.0)).collect(),
            })),
            HoldState::Blocked => {
                let remedy = match final_.remedy {
                    Some(Remedy::Steer(s)) => s,
                    Some(Remedy::HandEdit(h)) => format!("hand-edit:{}", h.0),
                    None => "blocked".to_string(),
                };
                Err(ErrorData::invalid_request(
                    format!("task rejected: {remedy}"),
                    None,
                ))
            }
            other => Err(ErrorData::internal_error(
                format!("non-terminal gate state: {other:?}"),
                None,
            )),
        }
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
