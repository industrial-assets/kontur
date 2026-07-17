use thiserror::Error;

use crate::audit::record::GateRecord;
use crate::canonical::canonical_bytes;
use crate::ids::{Hash, OperatorId};
use crate::sign::verify;
use crate::verdict::{SignedContent, Verdict};

/// The genesis anchor: the `prev_hash` of the first real record.
pub const GENESIS: Hash = Hash([0u8; 32]);

/// An append-only chain of gate records.
#[derive(Clone, Debug, Default)]
pub struct AuditChain {
    records: Vec<GateRecord>,
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum ChainError {
    #[error("record's prev_hash does not match the chain head")]
    WrongPrevHash,
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum ChainBreak {
    #[error("record {0} hash does not match its contents")]
    HashMismatch(usize),
    #[error("record {0} prev_hash does not match the previous record")]
    BrokenLink(usize),
    #[error("record {0} has an invalid checker signature")]
    BadCheckerSignature(usize),
}

impl AuditChain {
    pub fn new() -> Self {
        AuditChain { records: Vec::new() }
    }

    /// The hash to chain the next record onto: the last record's `this_hash`,
    /// or `GENESIS` when empty.
    pub fn head(&self) -> Hash {
        self.records
            .last()
            .map(|r| r.this_hash)
            .unwrap_or(GENESIS)
    }

    /// Append a record. Its `prev_hash` must equal the current head.
    pub fn append(&mut self, record: GateRecord) -> Result<(), ChainError> {
        if record.core.prev_hash != self.head() {
            return Err(ChainError::WrongPrevHash);
        }
        self.records.push(record);
        Ok(())
    }

    pub fn records(&self) -> &[GateRecord] {
        &self.records
    }
}

/// Verify an entire chain: every record's hash matches its contents, every link
/// matches the previous record, and every checker signature verifies. Any byte
/// mutation anywhere fails this (invariant #6).
pub fn verify_chain(records: &[GateRecord]) -> Result<(), ChainBreak> {
    let mut expected_prev = GENESIS;
    for (i, rec) in records.iter().enumerate() {
        if rec.recompute_hash() != rec.this_hash {
            return Err(ChainBreak::HashMismatch(i));
        }
        if rec.core.prev_hash != expected_prev {
            return Err(ChainBreak::BrokenLink(i));
        }
        for checker in &rec.core.checkers {
            let content = SignedContent {
                gate_id: rec.core.gate_id.clone(),
                diff_hash: rec.core.provenance.diff_hash,
                operator: checker.operator,
                verdict: checker.verdict.clone(),
                depth: checker.depth,
                cast_at: checker.cast_at,
            };
            if !verify(checker.operator, &canonical_bytes(&content), &checker.signature) {
                return Err(ChainBreak::BadCheckerSignature(i));
            }
        }
        expected_prev = rec.this_hash;
    }
    Ok(())
}

/// The operators whose verified `go` signatures back this record — the source
/// of the `Reviewed-by:` trailers (FR-21).
pub fn reviewed_by(record: &GateRecord) -> Vec<OperatorId> {
    record
        .core
        .checkers
        .iter()
        .filter(|c| {
            c.verdict == Verdict::Go && {
                let content = SignedContent {
                    gate_id: record.core.gate_id.clone(),
                    diff_hash: record.core.provenance.diff_hash,
                    operator: c.operator,
                    verdict: c.verdict.clone(),
                    depth: c.depth,
                    cast_at: c.cast_at,
                };
                verify(c.operator, &canonical_bytes(&content), &c.signature)
            }
        })
        .map(|c| c.operator)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::record::{GateRecord, Provenance};
    use crate::eligibility::MakerSet;
    use crate::hold::DualHold;
    use crate::ids::{GateId, TaskId};
    use crate::policy::Authorship;
    use crate::sign::{Ed25519Signer, FixedClock, Signer};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth};

    fn record(prev: Hash, gate: &str) -> GateRecord {
        let mut h = DualHold::new(
            GateId(gate.into()),
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
        let prov = Provenance {
            task_id: TaskId("t1".into()),
            prompt: "p".into(),
            prompt_author: Ed25519Signer::from_seed([1; 32]).operator_id(),
            agent_id: "a".into(),
            agent_model: "m".into(),
            agent_version: "v".into(),
            diff_hash: Hash([9u8; 32]),
            files: vec!["f".into()],
            loc: 1,
            tokens: 1,
        };
        GateRecord::build(prev, prov, &h).unwrap()
    }

    #[test]
    fn append_and_verify_two_record_chain() {
        let mut chain = AuditChain::new();
        let r1 = record(GENESIS, "g1");
        chain.append(r1).unwrap();
        let r2 = record(chain.head(), "g2");
        chain.append(r2).unwrap();
        assert!(verify_chain(chain.records()).is_ok());
        assert_eq!(chain.records().len(), 2);
    }

    #[test]
    fn append_rejects_wrong_prev_hash() {
        let mut chain = AuditChain::new();
        let bad = record(Hash([7u8; 32]), "g1"); // prev != GENESIS
        assert_eq!(chain.append(bad).unwrap_err(), ChainError::WrongPrevHash);
    }

    #[test]
    fn mutating_a_record_breaks_verification() {
        let mut chain = AuditChain::new();
        chain.append(record(GENESIS, "g1")).unwrap();
        let mut records = chain.records().to_vec();
        // Tamper with recorded provenance without recomputing the hash.
        records[0].core.provenance.loc = 999;
        assert_eq!(verify_chain(&records).unwrap_err(), ChainBreak::HashMismatch(0));
    }

    #[test]
    fn reviewed_by_lists_both_go_signers() {
        let r = record(GENESIS, "g1");
        let signers = reviewed_by(&r);
        assert_eq!(signers.len(), 2);
        assert!(signers.contains(&Ed25519Signer::from_seed([1; 32]).operator_id()));
        assert!(signers.contains(&Ed25519Signer::from_seed([2; 32]).operator_id()));
    }
}
