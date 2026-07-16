use serde::{Deserialize, Serialize};

use crate::canonical::canonical_bytes;
use crate::ids::{GateId, HandEditRef, Hash, OperatorId, Sig, Timestamp};
use crate::sign::{verify, Clock, Signer};

/// The corrective payload a `NoGo` must carry. Invariant #4: a `NoGo` cannot
/// exist without a remedy, so a bare veto is not representable.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Remedy {
    /// A corrective prompt sent back to the agent.
    Steer(String),
    /// A reference to a direct human change.
    HandEdit(HandEditRef),
}

/// An operator's decision at a gate.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Verdict {
    Go,
    NoGo(Remedy),
}

impl Verdict {
    pub fn is_go(&self) -> bool {
        matches!(self, Verdict::Go)
    }
}

/// How deeply the checker reviewed, captured for the audit record (PRD §9).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ReviewDepth {
    FullDiff,
    Summary,
    TestsRun,
}

/// Exactly the content an operator signs. Kept separate from `CastVerdict` so
/// the signed bytes are unambiguous and reproducible.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SignedContent {
    pub gate_id: GateId,
    pub diff_hash: Hash,
    pub operator: OperatorId,
    pub verdict: Verdict,
    pub depth: ReviewDepth,
    pub cast_at: Timestamp,
}

/// A verdict an operator has cast and signed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CastVerdict {
    pub operator: OperatorId,
    pub verdict: Verdict,
    pub depth: ReviewDepth,
    pub comment: Option<String>,
    pub cast_at: Timestamp,
    pub signature: Sig,
}

impl CastVerdict {
    /// Build and sign a verdict. `signer` provides both the identity and the
    /// signature; `clock` stamps the cast time (no wall-clock in the core).
    pub fn create(
        signer: &dyn Signer,
        clock: &dyn Clock,
        gate_id: &GateId,
        diff_hash: Hash,
        verdict: Verdict,
        depth: ReviewDepth,
        comment: Option<String>,
    ) -> Self {
        let operator = signer.operator_id();
        let cast_at = clock.now();
        let content = SignedContent {
            gate_id: gate_id.clone(),
            diff_hash,
            operator,
            verdict: verdict.clone(),
            depth,
            cast_at,
        };
        let signature = signer.sign(&canonical_bytes(&content));
        CastVerdict {
            operator,
            verdict,
            depth,
            comment,
            cast_at,
            signature,
        }
    }

    /// Verify this verdict's signature against its stated operator and the gate
    /// it belongs to. `gate_id` and `diff_hash` come from the hold, not the
    /// verdict, so a verdict cannot be replayed onto a different gate.
    pub fn verify_signature(&self, gate_id: &GateId, diff_hash: Hash) -> bool {
        let content = SignedContent {
            gate_id: gate_id.clone(),
            diff_hash,
            operator: self.operator,
            verdict: self.verdict.clone(),
            depth: self.depth,
            cast_at: self.cast_at,
        };
        verify(self.operator, &canonical_bytes(&content), &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::HandEditRef;

    #[test]
    fn nogo_always_carries_a_remedy() {
        // A NoGo must be constructed with a Remedy — there is no bare-veto variant.
        let v = Verdict::NoGo(Remedy::Steer("cache the token lookup".into()));
        assert!(!v.is_go());
        match v {
            Verdict::NoGo(Remedy::Steer(s)) => assert_eq!(s, "cache the token lookup"),
            _ => panic!("expected a steer remedy"),
        }

        let v2 = Verdict::NoGo(Remedy::HandEdit(HandEditRef("edit-1".into())));
        assert!(!v2.is_go());
    }

    #[test]
    fn go_is_go() {
        assert!(Verdict::Go.is_go());
    }
}
