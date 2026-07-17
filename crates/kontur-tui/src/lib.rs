//! Kontur TUI: the brutalist two-seat operator console (first slice).

pub mod view;
pub mod fleet;

pub use view::{
    ActiveRegion, AgentCard, AuditSummary, Banner, GateCard, InterventionCard, KeyStatus, KeyView,
    LogLine, Role, SessionView, Station, StatusStrip,
};
pub use fleet::{FleetSource, MockFleet};
