use crate::ids::OperatorId;
use crate::verdict::{CastVerdict, Verdict};

/// A cast verdict whose value is hidden while `sealed` is true (blind review,
/// invariant #3). The operator identity is always visible (needed for
/// deduplication and eligibility); the *verdict* is not.
#[derive(Clone)]
pub struct SealedVerdict {
    cv: CastVerdict,
    sealed: bool,
}

impl std::fmt::Debug for SealedVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("SealedVerdict");
        s.field("operator", &self.cv.operator);
        if self.sealed {
            s.field("verdict", &"<sealed>");
        } else {
            s.field("verdict", &self.cv.verdict);
        }
        s.field("sealed", &self.sealed).finish()
    }
}

impl SealedVerdict {
    pub fn new(cv: CastVerdict, sealed: bool) -> Self {
        SealedVerdict { cv, sealed }
    }

    pub fn operator(&self) -> OperatorId {
        self.cv.operator
    }

    /// The only public way to read the verdict value — returns `None` while
    /// sealed. Logs, queries, and API responses must go through this.
    pub fn reveal(&self) -> Option<&CastVerdict> {
        if self.sealed {
            None
        } else {
            Some(&self.cv)
        }
    }

    /// Crate-internal access for the hold's own evaluation logic. Not public,
    /// so no external caller can bypass the seal.
    pub(crate) fn raw(&self) -> &CastVerdict {
        &self.cv
    }

    pub(crate) fn unseal(&mut self) {
        self.sealed = false;
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
}

/// What an external observer is permitted to see about a cast verdict.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VerdictStatus {
    Sealed,
    Revealed(Verdict),
}

/// A projection of a cast verdict safe to show/log.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerdictView {
    pub operator: OperatorId,
    pub status: VerdictStatus,
}

impl SealedVerdict {
    pub fn view(&self) -> VerdictView {
        VerdictView {
            operator: self.operator(),
            status: match self.reveal() {
                Some(cv) => VerdictStatus::Revealed(cv.verdict.clone()),
                None => VerdictStatus::Sealed,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GateId, Hash};
    use crate::sign::{Ed25519Signer, FixedClock};
    use crate::verdict::CastVerdict;
    use crate::{ReviewDepth, Verdict};

    fn a_cast() -> CastVerdict {
        let signer = Ed25519Signer::from_seed([1u8; 32]);
        let clock = FixedClock(1000);
        CastVerdict::create(
            &signer,
            &clock,
            &GateId("g1".into()),
            Hash([0u8; 32]),
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }

    #[test]
    fn sealed_hides_value_but_shows_operator() {
        let cv = a_cast();
        let op = cv.operator;
        let sv = SealedVerdict::new(cv, true);
        assert_eq!(sv.operator(), op);
        assert!(sv.reveal().is_none());
        assert_eq!(sv.view().status, VerdictStatus::Sealed);
    }

    #[test]
    fn unseal_reveals_value() {
        let cv = a_cast();
        let mut sv = SealedVerdict::new(cv, true);
        sv.unseal();
        assert!(sv.reveal().is_some());
        assert_eq!(sv.view().status, VerdictStatus::Revealed(Verdict::Go));
    }

    #[test]
    fn debug_redacts_sealed_verdict() {
        let cv = a_cast();
        let sealed = SealedVerdict::new(cv.clone(), true);
        assert!(format!("{:?}", sealed).contains("<sealed>"));
        assert!(!format!("{:?}", sealed).contains("Go"));
        let open = SealedVerdict::new(cv, false);
        assert!(format!("{:?}", open).contains("Go"));
    }

    #[test]
    fn signature_roundtrips_and_binds_to_gate() {
        let cv = a_cast();
        assert!(cv.verify_signature(&GateId("g1".into()), Hash([0u8; 32])));
        // Replaying onto a different gate fails.
        assert!(!cv.verify_signature(&GateId("g2".into()), Hash([0u8; 32])));
    }
}
