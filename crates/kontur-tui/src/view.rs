use kontur_core::OperatorId;

/// Operator role. Structural: the Host's terminal runs the session; the Operator joins remotely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Host,
    Operator,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::Host => "HOST",
            Role::Operator => "OPERATOR",
        }
    }
}

/// A human seat.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Station {
    pub label: String,
    pub role: Role,
    pub activity: String,
    pub operator: OperatorId,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Banner {
    pub session: String,
    pub version: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StatusStrip {
    pub linked: bool,
    pub four_eyes: bool,
    pub fleet_count: usize,
    pub needs_you: usize,
    pub tokens: u64,
}

/// One agent panel on the watch-floor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentCard {
    pub id: String,
    pub status: String,
    pub tokens: u64,
    pub needs_signoff: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogLine {
    pub time: String,
    pub who: String,
    pub text: String,
}

/// What a key shows. Never carries a sealed verdict's value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyStatus {
    Awaiting,
    Sealed,
    Go,
    NoGo,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyView {
    pub label: String,
    pub role: Role,
    pub status: KeyStatus,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GateCard {
    pub gate_id: String,
    pub task: String,
    pub files: Vec<String>,
    pub loc: u32,
    pub keys: Vec<KeyView>,
    pub escalation_required: bool,
    pub diff_preview: Option<String>,
    /// FR-24: whether the operator has opened the diff pane for this gate.
    /// Must be set by the run loop after calling wire_to_view; render uses it
    /// to gate the [g] go key hint and refuse the cast.
    pub diff_opened: bool,
}

/// A no-go remedy being composed at a gate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InterventionCard {
    pub gate_id: String,
    pub steer: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuditSummary {
    pub gates: usize,
    pub reviewers: Vec<String>,
    pub chain_verified: bool,
    pub merged: bool,
    pub abandoned: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ActiveRegion {
    Idle,
    Prompt { prompt: String, ready: [bool; 2] },
    Plan { tasks: Vec<String>, ready: [bool; 2] },
    Gate(GateCard),
    Intervention(InterventionCard),
    SessionClosed(AuditSummary),
}

/// The full pure snapshot the console renders.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionView {
    pub banner: Banner,
    pub status: StatusStrip,
    pub stations: [Station; 2],
    pub fleet: Vec<AgentCard>,
    pub log: Vec<LogLine>,
    pub active: ActiveRegion,
    /// Host-side only: the operator invite, shown loudly while the second
    /// station is unlinked and hidden the moment both stations link.
    pub invite: Option<String>,
    /// Transient notice shown on the command row (bold) for a few frames —
    /// e.g. rejection hints or confirm prompts. None → plain " > " prompt.
    pub notice: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_labels() {
        assert_eq!(Role::Host.label(), "HOST");
        assert_eq!(Role::Operator.label(), "OPERATOR");
    }
}
