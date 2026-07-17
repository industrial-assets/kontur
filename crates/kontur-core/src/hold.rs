use serde::{Deserialize, Serialize};

use crate::eligibility::{is_eligible, MakerSet};
use crate::error::CastRejected;
use crate::ids::{GateId, Hash, TaskId};
use crate::policy::{Authorship, GatePolicy, Outcome};
use crate::sealed::{SealedVerdict, VerdictView};
use crate::verdict::CastVerdict;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum HoldState {
    Open,
    Partial,
    Satisfied,
    Blocked,
}

/// The result of accepting a cast: the new state plus whether the hold now
/// needs escalation (strict independence with too few eligible operators).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HoldOutcome {
    pub state: HoldState,
    pub escalation_required: bool,
}

/// The dual-hold: internals of the `AWAITING_REVIEW` lifecycle state. One per
/// gated action. Reaches `Satisfied` only on two `go` verdicts from two
/// distinct eligible operators (invariant #1); never clears on one key
/// (invariant #7).
#[derive(Clone, Debug)]
pub struct DualHold {
    gate_id: GateId,
    task_id: TaskId,
    diff_hash: Hash,
    policy: GatePolicy,
    makers: MakerSet,
    authorship: Authorship,
    verdicts: Vec<SealedVerdict>,
    version: u64,
    state: HoldState,
    contested: bool,
    outcome: Option<Outcome>,
}

impl DualHold {
    pub fn new(
        gate_id: GateId,
        task_id: TaskId,
        diff_hash: Hash,
        policy: GatePolicy,
        makers: MakerSet,
        authorship: Authorship,
    ) -> Self {
        DualHold {
            gate_id,
            task_id,
            diff_hash,
            policy,
            makers,
            authorship,
            verdicts: Vec::new(),
            version: 0,
            state: HoldState::Open,
            contested: false,
            outcome: None,
        }
    }

    /// A hold re-opened after an intervention (rejection or hand-edit). Marks
    /// the gate contested so a later clear records `ResolvedAfterDisagreement`.
    pub fn reopen(
        gate_id: GateId,
        task_id: TaskId,
        diff_hash: Hash,
        policy: GatePolicy,
        makers: MakerSet,
        authorship: Authorship,
    ) -> Self {
        let mut h = DualHold::new(gate_id, task_id, diff_hash, policy, makers, authorship);
        h.contested = true;
        h
    }

    pub fn state(&self) -> HoldState {
        self.state
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn gate_id(&self) -> &GateId {
        &self.gate_id
    }

    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    pub fn diff_hash(&self) -> Hash {
        self.diff_hash
    }

    pub fn authorship(&self) -> Authorship {
        self.authorship
    }

    pub fn policy(&self) -> GatePolicy {
        self.policy
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// Externally observable verdicts — sealed values stay hidden.
    pub fn observed_verdicts(&self) -> Vec<VerdictView> {
        self.verdicts.iter().map(SealedVerdict::view).collect()
    }

    /// Crate-internal: the raw cast verdicts, for building the audit record
    /// once the hold has resolved (Task 11).
    #[allow(dead_code)]
    pub(crate) fn raw_verdicts(&self) -> &[SealedVerdict] {
        &self.verdicts
    }

    // Called by Task 10 (hand-edit eligibility) and Task 11 (audit record).
    #[allow(dead_code)]
    pub(crate) fn makers(&self) -> &MakerSet {
        &self.makers
    }

    #[allow(dead_code)]
    pub(crate) fn contested(&self) -> bool {
        self.contested
    }

    /// Cast a signed verdict. See `CastRejected` for refusal reasons. On the
    /// second eligible verdict, evaluates the hold (blind: both hidden until
    /// now; non-blind: incremental).
    pub fn cast(
        &mut self,
        expected_version: u64,
        cv: CastVerdict,
    ) -> Result<HoldOutcome, CastRejected> {
        if matches!(self.state, HoldState::Satisfied | HoldState::Blocked) {
            return Err(CastRejected::AlreadyResolved);
        }
        if expected_version != self.version {
            return Err(CastRejected::StaleVersion {
                expected: expected_version,
                actual: self.version,
            });
        }
        if !cv.verify_signature(&self.gate_id, self.diff_hash) {
            return Err(CastRejected::BadSignature);
        }
        if self.verdicts.iter().any(|v| v.operator() == cv.operator) {
            return Err(CastRejected::DuplicateIdentity);
        }
        if !is_eligible(self.policy.independence, &self.makers, cv.operator) {
            return Err(CastRejected::Ineligible);
        }

        // Accept.
        let sealed = self.policy.blind;
        self.verdicts.push(SealedVerdict::new(cv, sealed));
        self.version += 1;

        self.evaluate();
        Ok(HoldOutcome {
            state: self.state,
            escalation_required: self.escalation_required(),
        })
    }

    /// Recompute state from the accumulated verdicts.
    fn evaluate(&mut self) {
        let have = self.verdicts.len() as u8;
        let required = self.policy.required;

        // In blind mode we defer any decision until all required verdicts are
        // in, so the second reviewer can never observe the first (not even
        // "it was a no-go"). In non-blind mode a no-go short-circuits.
        if !self.policy.blind {
            if self.verdicts.iter().any(|v| !v.raw().verdict.is_go()) {
                self.block();
                return;
            }
        }

        if have < required {
            self.state = HoldState::Partial;
            return;
        }

        // All required verdicts present — reveal and decide.
        for v in &mut self.verdicts {
            v.unseal();
        }
        if self.verdicts.iter().all(|v| v.raw().verdict.is_go()) {
            self.state = HoldState::Satisfied;
            self.outcome = Some(if self.contested {
                Outcome::ResolvedAfterDisagreement
            } else {
                Outcome::Unanimous
            });
        } else {
            self.block();
        }
    }

    fn block(&mut self) {
        for v in &mut self.verdicts {
            v.unseal();
        }
        self.state = HoldState::Blocked;
    }

    /// Strict independence with fewer eligible operators than required sign
    /// keys cannot clear — the caller must escalate (invariant #7). This is a
    /// signal only; the core runs no timer.
    fn escalation_required(&self) -> bool {
        false // refined in Task 10 when hand-edit shrinks the eligible pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Hash;
    use crate::sign::{Ed25519Signer, FixedClock, Signer};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth, Verdict, VerdictStatus};

    fn hold() -> DualHold {
        DualHold::new(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Agent,
        )
    }

    fn go(seed: u8, h: &DualHold) -> CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        let clock = FixedClock(1000 + seed as i64);
        CastVerdict::create(
            &signer,
            &clock,
            h.gate_id(),
            h.diff_hash(),
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        )
    }

    #[test]
    fn two_distinct_go_reaches_satisfied() {
        let mut h = hold();
        assert_eq!(h.state(), HoldState::Open);

        let v = h.cast(0, go(1, &h)).unwrap();
        assert_eq!(v.state, HoldState::Partial);

        let v = h.cast(1, go(2, &h)).unwrap();
        assert_eq!(v.state, HoldState::Satisfied);
        assert_eq!(h.outcome(), Some(Outcome::Unanimous));
    }

    #[test]
    fn blind_hides_first_verdict_until_second_in() {
        let mut h = hold(); // default blind = true
        let signer1 = Ed25519Signer::from_seed([1; 32]);
        h.cast(0, go(1, &h)).unwrap();

        // While Partial+blind, the first verdict's value is not observable.
        let views = h.observed_verdicts();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].operator, signer1.operator_id());
        assert_eq!(views[0].status, VerdictStatus::Sealed);

        // After the second, both reveal.
        h.cast(1, go(2, &h)).unwrap();
        for view in h.observed_verdicts() {
            assert_eq!(view.status, VerdictStatus::Revealed(Verdict::Go));
        }
    }
}
