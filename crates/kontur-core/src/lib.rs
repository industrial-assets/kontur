//! Kontur core: the four-eyes dual-hold gate and tamper-evident audit chain.
//!
//! Pure, synchronous, no I/O. Time and signing are injected via traits.

pub mod audit;
pub mod canonical;
pub mod eligibility;
pub mod error;
pub mod hold;
pub mod ids;
pub mod policy;
pub mod sealed;
pub mod sign;
pub mod verdict;

pub use audit::{CheckerEntry, GateRecord, Provenance, RecordCore, RecordError};
pub use canonical::{canonical_bytes, sha256};
pub use eligibility::{is_eligible, MakerSet};
pub use error::CastRejected;
pub use hold::{DualHold, HoldOutcome, HoldState};
pub use ids::{GateId, HandEditRef, Hash, OperatorId, Sig, TaskId, Timestamp};
pub use policy::{Authorship, Availability, GatePolicy, Independence, Outcome};
pub use sealed::{SealedVerdict, VerdictStatus, VerdictView};
pub use sign::{verify, Clock, Ed25519Signer, Signer};
pub use verdict::{CastVerdict, Remedy, ReviewDepth, SignedContent, Verdict};
