//! Kontur MCP enforcement plane: gates an agent's task-completion boundary
//! through the four-eyes `kontur-core` engine and emits the audit record.

pub mod error;
pub mod gatehost;
pub mod session;
pub mod workspace;
pub mod provenance;

pub use error::{GateHostError, WorkspaceError};
pub use gatehost::{GateFinal, GateHost, GateProgress, GateView};
pub use session::SessionContext;
pub use workspace::{diff_hash, CommandOutput, FrozenDiff, InMemoryWorkspace, Workspace};
pub use provenance::build_provenance;
