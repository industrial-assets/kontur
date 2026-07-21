use serde::{Deserialize, Serialize};

/// Whether the change's maker may also be a checker.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Independence {
    /// The maker (prompt author / hand-editor) may not cast a counting verdict.
    Strict,
    /// The maker may be one of the two, but the co-signer must be a non-maker.
    Pragmatic,
}

/// What happens when two eligible keys cannot be gathered. There is exactly
/// one policy: park. Kontur deliberately has **no third signatory** — if two
/// operators cannot agree, resolving that is theirs to do, not the system's to
/// route around. (`escalation_required` on a hold is a different, unrelated
/// signal: it flags that the co-signer must be a *distinct* non-maker for
/// invariant #7, not that a third human is needed.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Availability {
    /// Hold parks indefinitely (safe default) — never degrade to one key,
    /// never escalate past the two seats.
    Park,
}

/// Provenance of the change under review.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Authorship {
    Agent,
    HandEdited,
    Both,
}

/// How a resolved gate concluded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Outcome {
    Unanimous,
    ResolvedAfterDisagreement,
    /// The gate resolved with a no-go; the dissenting checker entry carries the
    /// remedy and its signature. The task routed to intervention.
    Blocked,
}

/// The rules governing a single gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GatePolicy {
    /// Required signatories. Fixed at 2 for MVP; typed so it can't silently drift.
    pub required: u8,
    pub independence: Independence,
    /// Seal the first verdict until both are in (blind second review).
    pub blind: bool,
    pub availability: Availability,
}

impl Default for GatePolicy {
    fn default() -> Self {
        GatePolicy {
            required: 2,
            independence: Independence::Strict,
            blind: true,
            availability: Availability::Park,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_strict_blind_park() {
        let p = GatePolicy::default();
        assert_eq!(p.required, 2);
        assert_eq!(p.independence, Independence::Strict);
        assert!(p.blind);
        assert_eq!(p.availability, Availability::Park);
    }
}
