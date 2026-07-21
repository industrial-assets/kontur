use kontur_core::{CastVerdict, GateId, Hash, OperatorId, VerdictView};
use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on any incompatible change to the message
/// types below. A client that omits the field (pre-versioning build) is read
/// as version 0, which mismatches any real version and is rejected cleanly.
pub const PROTOCOL_VERSION: u32 = 5;

/// Serde default for `Hello.protocol` — pre-versioning clients deserialize to 0.
fn protocol_v0() -> u32 {
    0
}

/// Operator role transmitted on the wire. An enum prevents the casing-mismatch
/// bug where the server emits `"Host"` but the client compared `== "HOST"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireRole {
    Host,
    Operator,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    Hello {
        seat: String,
        operator: OperatorId,
        /// The client's wire protocol version. Defaults to 0 when absent so an
        /// old build is rejected with a clear message rather than a serde error.
        #[serde(default = "protocol_v0")]
        protocol: u32,
    },
    Ready,
    Cast {
        gate_id: GateId,
        verdict: CastVerdict,
    },
    HandEdit {
        path: String,
        contents: String,
    },
    Abandon,
    Bye,
    /// Toggle a soft presence claim on a gate: "I'm reviewing this one".
    /// Sets the claim to this seat, or clears it if this seat already holds it.
    /// Presence only — never affects verdict eligibility or the four-eyes hold.
    Claim {
        gate_id: GateId,
    },
    /// Append a note to a gate's discussion thread. Communication only —
    /// visible to both seats, never affects the four-eyes hold.
    Discuss {
        gate_id: GateId,
        text: String,
    },
    /// Answer a clarification question. Both seats answer every question; the
    /// exchange resolves (and the agent is unblocked) when all are settled.
    Answer {
        question: usize,
        choice: WireChoice,
    },
    /// Application-level keepalive. Sent periodically by the client; the server
    /// treats its arrival as liveness and replies `Pong`. Never gated.
    Ping,
    SetPrompt {
        prompt: String,
    },
    /// Live draft of the prompt as a seat types, one message per keystroke.
    /// Valid only during dispatch composition; updates the shared prompt and
    /// resets both ready flags (same anchoring rule as `SetPrompt`) but is
    /// never logged — the commit (`SetPrompt`) is the logged event. May be
    /// empty (mid-edit); the dispatch gate refuses an empty prompt anyway.
    /// Simultaneous drafts from both seats are last-write-wins.
    PromptDraft {
        prompt: String,
    },
    /// Request the current on-disk contents of a worktree file.
    /// Response arrives as `ServerMsg::FileContent` on the same connection.
    FetchFile {
        path: String,
    },
    /// Replace the current plan with a new task list. Valid only during
    /// `PlanReview`; resets both ready flags so both seats must re-consent.
    EditPlan {
        tasks: Vec<String>,
    },
    /// Send a steer prompt to the agent to revise its plan. Valid only during
    /// `PlanReview`; resets both ready flags and withdraws the current plan list.
    SteerPlan {
        steer: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ServerMsg {
    Welcome {
        seat: String,
    },
    State(Box<WireState>),
    Rejected {
        reason: String,
    },
    /// Reply to `ClientMsg::FetchFile`.  `contents` is `None` when the path
    /// does not exist in the worktree (new file) or the file is binary (binary
    /// round-trip via text editor is out of scope; edit locally on the host).
    FileContent {
        path: String,
        contents: Option<String>,
    },
    /// Reply to a client `Ping`, so a client can detect a dead host too.
    Pong,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireSeat {
    pub label: String,
    pub operator: OperatorId,
    pub role: WireRole,
    pub linked: bool,
    pub ready: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WirePhase {
    AwaitOperators,
    DispatchReady {
        prompt: String,
    },
    PlanReview {
        tasks: Vec<String>,
    },
    /// The agent asked the operators to clarify ambiguity before planning.
    Clarify {
        questions: Vec<WireQuestion>,
    },
    Executing,
    Closed {
        gates: usize,
        chain_verified: bool,
        reviewers: Vec<String>,
        merged: bool,
        abandoned: bool,
    },
}

/// One clarification question projected for the console: the current prompt and
/// options (which change during reconciliation), whether a free-text answer is
/// offered, and each seat's current pick as display text (live to both).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireQuestion {
    pub prompt: String,
    pub options: Vec<String>,
    pub allows_custom: bool,
    /// [seat A pick, seat B pick] as display text; None until that seat answers.
    pub picks: [Option<String>; 2],
    /// The resolved answer(s), once this question is settled.
    pub resolved: Option<Vec<String>>,
}

/// A seat's answer to a clarification question.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WireChoice {
    Option(usize),
    Custom(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireFleetCard {
    pub id: String,
    pub status: String,
    pub needs_signoff: bool,
}

/// Wire representation of a gate.
/// The `keys` field holds `VerdictView`s which are sealing-safe by construction:
/// a sealed verdict carries only the operator identity and status (Sealed),
/// never the actual Go/NoGo choice.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireGate {
    pub gate_id: GateId,
    pub task: String,
    pub files: Vec<String>,
    pub loc: u32,
    pub diff_hash: Hash,
    pub keys: Vec<VerdictView>,
    pub escalation_required: bool,
    /// The gate diff, split per file, each section independently capped at
    /// 64 KiB — one huge generated file cannot starve the others' diffs.
    pub file_diffs: Vec<WireFileDiff>,
    /// True when any file's section was capped. Operators who approve a
    /// truncated diff must explicitly acknowledge before their `go` is cast.
    pub diff_truncated: bool,
    /// The task's most recent command and its exit code — the closest thing
    /// to a test result the review surface can show truthfully.
    pub last_cmd: Option<WireCmd>,
    /// Seat label of the operator currently reviewing this gate, if claimed.
    /// A soft presence signal (PRD FR-3), not a lock.
    pub claimed_by: Option<String>,
    /// Gate-anchored discussion notes, in order. Communication only.
    pub discuss: Vec<WireComment>,
}

/// One note in a gate's discussion thread.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireComment {
    pub who: String,
    pub text: String,
}

/// A completed command and its outcome.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireCmd {
    pub command: String,
    pub exit_code: i32,
}

/// One file's diff section at a gate.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireFileDiff {
    pub path: String,
    pub diff: String,
    /// True when this section was capped at 64 KiB on the server.
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireState {
    pub phase: WirePhase,
    pub seats: Vec<WireSeat>,
    pub fleet: Vec<WireFleetCard>,
    pub log: Vec<String>,
    pub gate: Option<WireGate>,
    /// The session instruction, carried in every phase so it stays visible
    /// after dispatch (during plan review and execution), not only while it
    /// is being composed at the dispatch gate.
    pub prompt: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kontur_core::VerdictStatus;

    #[test]
    fn sealed_key_on_the_wire_carries_no_value() {
        let view = VerdictView {
            operator: OperatorId([1; 32]),
            status: VerdictStatus::Sealed,
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("Sealed"));
        assert!(!json.contains("Revealed"));
        assert!(!json.contains("\"Go\""));
        assert!(!json.contains("NoGo"));
    }
}
