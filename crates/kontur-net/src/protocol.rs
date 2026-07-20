use serde::{Deserialize, Serialize};
use kontur_core::{CastVerdict, GateId, Hash, OperatorId, VerdictView};

/// Operator role transmitted on the wire. An enum prevents the casing-mismatch
/// bug where the server emits `"Host"` but the client compared `== "HOST"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireRole {
    Host,
    Operator,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    Hello { seat: String, operator: OperatorId },
    Ready,
    Cast { gate_id: GateId, verdict: CastVerdict },
    HandEdit { path: String, contents: String },
    Abandon,
    Bye,
    SetPrompt { prompt: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ServerMsg {
    Welcome { seat: String },
    State(Box<WireState>),
    Rejected { reason: String },
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
    DispatchReady { prompt: String },
    PlanReview { tasks: Vec<String> },
    Executing,
    Closed { gates: usize, chain_verified: bool, reviewers: Vec<String>, merged: bool, abandoned: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireFleetCard {
    pub id: String,
    pub status: String,
    pub tokens: u64,
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
    pub diff_preview: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireState {
    pub phase: WirePhase,
    pub seats: Vec<WireSeat>,
    pub fleet: Vec<WireFleetCard>,
    pub log: Vec<String>,
    pub gate: Option<WireGate>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kontur_core::VerdictStatus;

    #[test]
    fn sealed_key_on_the_wire_carries_no_value() {
        let view = VerdictView { operator: OperatorId([1; 32]), status: VerdictStatus::Sealed };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("Sealed"));
        assert!(!json.contains("Revealed"));
        assert!(!json.contains("\"Go\""));
        assert!(!json.contains("NoGo"));
    }
}
