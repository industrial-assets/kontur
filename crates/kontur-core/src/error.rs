use thiserror::Error;

/// Why a `cast` was refused. Every rejection is enforced at cast time, never
/// only at display (invariant #2).
#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum CastRejected {
    #[error("stale version: expected {expected}, hold is at {actual}")]
    StaleVersion { expected: u64, actual: u64 },
    #[error("this operator has already cast on this hold")]
    DuplicateIdentity,
    #[error("operator is not eligible to check this change (independence policy)")]
    Ineligible,
    #[error("hold is already resolved")]
    AlreadyResolved,
    #[error("verdict signature is invalid for this gate")]
    BadSignature,
}
