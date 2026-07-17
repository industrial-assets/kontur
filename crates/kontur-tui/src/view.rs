use kontur_core::OperatorId;

/// Operator role. Rotates in later slices; label-only here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Driver,
    Navigator,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::Driver => "DRIVER",
            Role::Navigator => "NAVIGATOR",
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
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ActiveRegion {
    Idle,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_labels() {
        assert_eq!(Role::Driver.label(), "DRIVER");
        assert_eq!(Role::Navigator.label(), "NAVIGATOR");
    }
}
