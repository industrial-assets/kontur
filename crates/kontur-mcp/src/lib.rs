//! Kontur MCP enforcement plane: gates an agent's task-completion boundary
//! through the four-eyes `kontur-core` engine and emits the audit record.

pub mod error;
pub mod fs_workspace;
pub mod gatehost;
pub mod git_workspace;
pub mod provenance;
pub mod server;
pub mod session;
pub mod workspace;

pub use error::{GateHostError, WorkspaceError};
pub use fs_workspace::FsWorkspace;
pub use gatehost::{GateFinal, GateHost, GateProgress, GateView, HostEvent, PlanDecision};
pub use git_workspace::GitWorkspace;
pub use provenance::build_provenance;
pub use server::KonturServer;
pub use session::SessionContext;
pub use workspace::{diff_hash, CommandOutput, FrozenDiff, InMemoryWorkspace, Workspace};
