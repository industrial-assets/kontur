use serde::{Deserialize, Serialize};

use crate::ids::OperatorId;
use crate::policy::Independence;

/// The set of operators who made this change (prompt author, hand-editor(s)).
/// Used to enforce independence at cast time (invariant #2).
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MakerSet(Vec<OperatorId>);

impl MakerSet {
    pub fn new() -> Self {
        MakerSet(Vec::new())
    }

    /// Builder-style add (deduplicates).
    pub fn with(mut self, op: OperatorId) -> Self {
        if !self.0.contains(&op) {
            self.0.push(op);
        }
        self
    }

    pub fn contains(&self, op: &OperatorId) -> bool {
        self.0.contains(op)
    }
}

/// Is `op` allowed to cast a counting verdict on a change made by `makers`?
///
/// - `Strict`: a maker may never check their own work.
/// - `Pragmatic`: a maker may cast; the *hold* still requires the co-signer to
///   be a distinct identity (enforced in `hold.rs`), so a lone maker can never
///   satisfy a gate alone.
pub fn is_eligible(independence: Independence, makers: &MakerSet, op: OperatorId) -> bool {
    match independence {
        Independence::Strict => !makers.contains(&op),
        Independence::Pragmatic => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(n: u8) -> OperatorId {
        OperatorId([n; 32])
    }

    #[test]
    fn strict_excludes_the_maker() {
        let makers = MakerSet::new().with(op(1));
        assert!(!is_eligible(Independence::Strict, &makers, op(1)));
        assert!(is_eligible(Independence::Strict, &makers, op(2)));
    }

    #[test]
    fn pragmatic_allows_the_maker() {
        let makers = MakerSet::new().with(op(1));
        assert!(is_eligible(Independence::Pragmatic, &makers, op(1)));
        assert!(is_eligible(Independence::Pragmatic, &makers, op(2)));
    }

    #[test]
    fn makerset_dedups() {
        let makers = MakerSet::new().with(op(1)).with(op(1));
        assert!(makers.contains(&op(1)));
    }
}
