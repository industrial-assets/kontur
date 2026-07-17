use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::canonical::{canonical_bytes, sha256};
use crate::hold::{DualHold, HoldState};
use crate::ids::{GateId, Hash, OperatorId, Sig, TaskId, Timestamp};
use crate::policy::{Authorship, Outcome};
use crate::verdict::{ReviewDepth, Verdict};

/// Provenance of the change (PRD §9). These fields originate upstream (prompt
/// co-construction, the agent adapter) and are supplied by the caller.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub task_id: TaskId,
    pub prompt: String,
    pub prompt_author: OperatorId,
    pub agent_id: String,
    pub agent_model: String,
    pub agent_version: String,
    pub diff_hash: Hash,
    pub files: Vec<String>,
    pub loc: u32,
    pub tokens: u64,
}

/// One checker's signed decision, as recorded.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CheckerEntry {
    pub operator: OperatorId,
    pub cast_at: Timestamp,
    pub verdict: Verdict,
    pub depth: ReviewDepth,
    pub comment: Option<String>,
    pub signature: Sig,
}

/// Everything in a gate record except its own hash — the bytes that get hashed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RecordCore {
    pub prev_hash: Hash,
    pub gate_id: GateId,
    pub provenance: Provenance,
    pub authorship: Authorship,
    pub checkers: Vec<CheckerEntry>,
    pub outcome: Outcome,
}

/// A signed, hash-chained gate record (PRD §9). Immutable once built.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GateRecord {
    pub core: RecordCore,
    pub this_hash: Hash,
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum RecordError {
    #[error("cannot record a gate that is not satisfied")]
    HoldNotSatisfied,
}

impl GateRecord {
    /// Build the record for a satisfied hold, chained to `prev_hash`. Only a
    /// satisfied hold (two go verdicts) produces a merge record; blocked holds
    /// route to intervention and are recorded by the caller separately.
    pub fn build(
        prev_hash: Hash,
        provenance: Provenance,
        hold: &DualHold,
    ) -> Result<GateRecord, RecordError> {
        if hold.state() != HoldState::Satisfied {
            return Err(RecordError::HoldNotSatisfied);
        }
        let outcome = hold.outcome().expect("satisfied hold has an outcome");

        let checkers: Vec<CheckerEntry> = hold
            .raw_verdicts()
            .iter()
            .map(|sv| {
                let cv = sv.raw();
                CheckerEntry {
                    operator: cv.operator,
                    cast_at: cv.cast_at,
                    verdict: cv.verdict.clone(),
                    depth: cv.depth,
                    comment: cv.comment.clone(),
                    signature: cv.signature,
                }
            })
            .collect();

        let core = RecordCore {
            prev_hash,
            gate_id: hold.gate_id().clone(),
            provenance,
            authorship: hold.authorship(),
            checkers,
            outcome,
        };
        let this_hash = sha256(&canonical_bytes(&core));
        Ok(GateRecord { core, this_hash })
    }

    /// Recompute the hash from the core — used by chain verification.
    pub fn recompute_hash(&self) -> Hash {
        sha256(&canonical_bytes(&self.core))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eligibility::MakerSet;
    use crate::ids::TaskId;
    use crate::sign::{Ed25519Signer, FixedClock, Signer};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth};

    fn satisfied_hold() -> DualHold {
        let mut h = DualHold::new(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Agent,
        );
        for seed in [1u8, 2u8] {
            let signer = Ed25519Signer::from_seed([seed; 32]);
            let clock = FixedClock(1000 + seed as i64);
            let cv = CastVerdict::create(
                &signer,
                &clock,
                h.gate_id(),
                h.diff_hash(),
                Verdict::Go,
                ReviewDepth::FullDiff,
                None,
            );
            let ev = h.version();
            h.cast(ev, cv).unwrap();
        }
        h
    }

    fn provenance() -> Provenance {
        Provenance {
            task_id: TaskId("t1".into()),
            prompt: "refactor session guard".into(),
            prompt_author: Ed25519Signer::from_seed([1; 32]).operator_id(),
            agent_id: "agent-03".into(),
            agent_model: "claude-opus-4-8".into(),
            agent_version: "1.0".into(),
            diff_hash: Hash([9u8; 32]),
            files: vec!["auth/session.ts".into()],
            loc: 59,
            tokens: 6400,
        }
    }

    #[test]
    fn build_records_two_checkers_and_hashes() {
        let h = satisfied_hold();
        let rec = GateRecord::build(Hash([0u8; 32]), provenance(), &h).unwrap();
        assert_eq!(rec.core.checkers.len(), 2);
        assert_eq!(rec.core.outcome, Outcome::Unanimous);
        assert_eq!(rec.this_hash, rec.recompute_hash());
    }

    #[test]
    fn refuses_unsatisfied_hold() {
        // A fresh, open hold — never satisfied.
        let h = DualHold::new(
            GateId("g2".into()),
            TaskId("t2".into()),
            Hash([1u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Agent,
        );
        let err = GateRecord::build(Hash([0u8; 32]), provenance(), &h).unwrap_err();
        assert_eq!(err, RecordError::HoldNotSatisfied);
    }
}
