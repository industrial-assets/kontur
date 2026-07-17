//! Kontur MCP enforcement plane: gates an agent's task-completion boundary
//! through the four-eyes `kontur-core` engine and emits the audit record.

pub mod error;
pub mod session;

pub use error::{GateHostError, WorkspaceError};
pub use session::SessionContext;
