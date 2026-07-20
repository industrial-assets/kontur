//! Kontur TUI: the brutalist two-seat operator console (first slice).

pub mod view;
pub mod fleet;
pub mod input;
pub mod viewmodel;
pub mod render;
pub mod app;
pub mod demo;
pub mod remote;
pub mod link;
pub mod claude_agent;

pub use view::{
    ActiveRegion, AgentCard, AuditSummary, Banner, GateCard, InterventionCard, KeyStatus, KeyView,
    LogLine, Role, SessionView, Station, StatusStrip,
};
pub use fleet::{FleetSource, MockFleet};
pub use input::{map_key, Action};
pub use viewmodel::build_session_view;
pub use render::render;
pub use app::{poll_action, TerminalGuard, Tui};
pub use remote::{wire_to_view, run_remote};
