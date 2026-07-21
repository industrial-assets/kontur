//! Kontur TUI: the brutalist two-seat operator console (first slice).

pub mod app;
pub mod boot;
pub mod claude_agent;
pub mod demo;
pub mod diffview;
pub mod fleet;
pub mod input;
pub mod link;
pub mod planedit;
pub mod remote;
pub mod render;
pub mod view;
pub mod viewmodel;

pub use app::{poll_action, TerminalGuard, Tui};
pub use fleet::{FleetSource, MockFleet};
pub use input::{map_key, Action};
pub use remote::{run_remote, wire_to_view};
pub use render::render;
pub use view::{
    ActiveRegion, AgentCard, AuditSummary, Banner, GateCard, InterventionCard, KeyStatus, KeyView,
    LogLine, Role, SessionView, Station, StatusStrip,
};
pub use viewmodel::build_session_view;
