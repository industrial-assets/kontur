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
    pub afk: bool,
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
}

/// One agent panel on the watch-floor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentCard {
    pub id: String,
    pub status: String,
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
    /// Per-file diff sections; the DIFF pane shows the tab-selected one.
    pub file_diffs: Vec<FileDiffView>,
    /// True when any file's section was truncated at 64 KiB on the server. A
    /// `go` on a truncated diff requires a second `g` press to acknowledge.
    pub diff_truncated: bool,
    /// The task's most recent command and exit code, shown on the verdict bar.
    pub last_cmd: Option<(String, i32)>,
    /// Seat label of the operator reviewing this gate (soft presence), if any.
    pub claimed_by: Option<String>,
    /// Gate-anchored discussion notes: (who, text) in order.
    pub discuss: Vec<(String, String)>,
}

/// One file's diff section at a gate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiffView {
    pub path: String,
    pub diff: String,
    pub truncated: bool,
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
    Prompt {
        prompt: String,
        ready: [bool; 2],
    },
    /// Plan review: operators approve, edit, reorder, or delete tasks before
    /// execution begins. `selected` is the currently highlighted row.
    Plan {
        tasks: Vec<String>,
        ready: [bool; 2],
        selected: usize,
    },
    Gate(GateCard),
    Intervention(InterventionCard),
    /// The agent asked clarifying questions; both operators answer. `selected`
    /// is the highlighted question row; `own` is this seat (0/1).
    Clarify {
        questions: Vec<ClarifyQ>,
        selected: usize,
        own: usize,
    },
    /// The agent proposed splitting into parallel streams; operators approve
    /// (both [y]) or decline ([n]).
    Split {
        agent: String,
        streams: Vec<(String, String)>,
        ready: [bool; 2],
    },
    SessionClosed(AuditSummary),
}

/// Where the text-entry cursor sits while composing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorTarget {
    /// On the command row: `col` is the 0-based column within that row.
    Command { col: u16 },
    /// In the PROMPT pane: `index` is the char offset into the draft.
    Prompt { index: usize },
}

/// One clarification question as shown to an operator.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClarifyQ {
    pub prompt: String,
    pub options: Vec<String>,
    pub allows_custom: bool,
    /// [seat A pick, seat B pick] display text; None until answered.
    pub picks: [Option<String>; 2],
    pub resolved: Option<Vec<String>>,
}

/// Operator-attention indicator: tells THIS seat what (if anything) it must do.
///
/// `loud=true`  → this seat must act NOW (BOLD + REVERSED).
/// `loud=false` → informational wait, e.g. the other seat is the blocker (DIM).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Attention {
    pub text: String,
    pub loud: bool,
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
    /// Per-seat attention line rendered directly below the status strip.
    /// loud=true → this seat must act NOW; loud=false → informational wait.
    /// None → no row (fleet/log already show activity; no line needed).
    pub attention: Option<Attention>,
    /// The dispatched instruction, shown as a TASK line above the fleet during
    /// plan review and execution so the ask stays visible after dispatch.
    /// None while composing (the PROMPT pane shows the draft) and at close.
    pub instruction: Option<String>,
    /// When true, a centred keymap overlay is drawn above the console.
    pub show_help: bool,
    /// Where the text-entry cursor should be drawn, while composing. Rendered
    /// only on the "on" frames of the blink cadence, giving a slow flash.
    pub cursor: Option<CursorTarget>,
    /// Whether this frame is a cursor-visible ("on") frame.
    pub blink_on: bool,
    /// Host-only: path to the agent's session log, shown as a persistent
    /// footer so the host can tail the agent's narration. None on the operator
    /// console (the log is host-local and unreachable from there).
    pub agent_log: Option<String>,
    /// True when the connection to the host has gone silent past the keepalive
    /// timeout — the session is frozen. Renders a loud banner in place of the
    /// identity header so the operator knows their casts won't land.
    pub link_lost: bool,
    /// Host-only: a BYO operator awaiting approval — the fingerprint to verify.
    /// Rendered as a loud approval prompt; None on the operator console.
    pub join_request: Option<String>,
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
