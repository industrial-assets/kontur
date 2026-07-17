use kontur_core::CastRejected;
use thiserror::Error;

/// Failures from the workspace port.
#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace io error: {0}")]
    Io(String),
    #[error("unknown task: {0}")]
    UnknownTask(String),
}

/// Failures from the gate host's operator/agent faces.
#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum GateHostError {
    #[error("unknown gate: {0}")]
    UnknownGate(String),
    #[error("verdict rejected: {0}")]
    Cast(#[from] CastRejected),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
}
