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
    eligible_pool: usize,
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
            eligible_pool: usize::MAX,
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

    /// A fresh hold opened after a hand-edit: authorship reflects human
    /// involvement, the editor joins the maker set (so strict mode excludes
    /// them), and the eligible pool is computed from the known operators. If
    /// that pool is smaller than the required keys, the hold reports
    /// `escalation_required` on the next cast (invariants #5, #7).
    pub fn reopen_handedit(
        gate_id: GateId,
        task_id: TaskId,
        diff_hash: Hash,
        policy: GatePolicy,
        prior_makers: MakerSet,
        editor: crate::ids::OperatorId,
        agent_authored: bool,
        known_operators: &[crate::ids::OperatorId],
    ) -> Self {
        let makers = prior_makers.with(editor);
        let authorship = if agent_authored {
            Authorship::Both
        } else {
            Authorship::HandEdited
        };
        let mut h = DualHold::reopen(gate_id, task_id, diff_hash, policy, makers.clone(), authorship);
        h.eligible_pool = match policy.independence {
            crate::policy::Independence::Strict => {
                known_operators.iter().filter(|op| !makers.contains(op)).count()
            }
            crate::policy::Independence::Pragmatic => known_operators.len(),
        };
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

    /// If the hold is blocked by a no-go, the remedy that must drive the
    /// rework/replan ripple. `None` unless blocked with a no-go verdict.
    pub fn blocking_remedy(&self) -> Option<crate::verdict::Remedy> {
        if self.state != HoldState::Blocked {
            return None;
        }
        self.verdicts.iter().find_map(|v| match &v.raw().verdict {
            crate::verdict::Verdict::NoGo(remedy) => Some(remedy.clone()),
            crate::verdict::Verdict::Go => None,
        })
    }

    /// Strict independence with fewer eligible operators than required keys
    /// cannot clear — the caller must escalate (invariant #7). Signal only.
    fn escalation_required(&self) -> bool {
        self.eligible_pool < self.policy.required as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Hash;
    use crate::sign::{Ed25519Signer, FixedClock, Signer};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth, Verdict, VerdictStatus};
    use crate::Remedy;

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

    fn strict_hold_with_maker(maker_seed: u8) -> DualHold {
        let maker = Ed25519Signer::from_seed([maker_seed; 32]).operator_id();
        DualHold::new(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(), // strict
            MakerSet::new().with(maker),
            Authorship::Agent,
        )
    }

    #[test]
    fn duplicate_identity_is_rejected() {
        let mut h = hold();
        h.cast(0, go(1, &h)).unwrap();
        let err = h.cast(1, go(1, &h)).unwrap_err();
        assert_eq!(err, CastRejected::DuplicateIdentity);
        // State and version unchanged by the rejected second cast.
        assert_eq!(h.state(), HoldState::Partial);
        assert_eq!(h.version(), 1);
    }

    #[test]
    fn stale_version_is_rejected() {
        let mut h = hold();
        h.cast(0, go(1, &h)).unwrap();
        let err = h.cast(0, go(2, &h)).unwrap_err(); // expected 1, not 0
        assert_eq!(
            err,
            CastRejected::StaleVersion { expected: 0, actual: 1 }
        );
    }

    #[test]
    fn strict_mode_rejects_the_maker() {
        let mut h = strict_hold_with_maker(1);
        let err = h.cast(0, go(1, &h)).unwrap_err();
        assert_eq!(err, CastRejected::Ineligible);
        // A non-maker is accepted.
        assert!(h.cast(0, go(2, &h)).is_ok());
    }

    #[test]
    fn cannot_cast_after_resolved() {
        let mut h = hold();
        h.cast(0, go(1, &h)).unwrap();
        h.cast(1, go(2, &h)).unwrap(); // Satisfied
        let err = h.cast(2, go(3, &h)).unwrap_err();
        assert_eq!(err, CastRejected::AlreadyResolved);
    }

    #[test]
    fn bad_signature_is_rejected() {
        let mut h = hold();
        // Sign for a *different* gate, then submit here — signature won't verify.
        let signer = Ed25519Signer::from_seed([1; 32]);
        let clock = FixedClock(1000);
        let forged = CastVerdict::create(
            &signer,
            &clock,
            &GateId("other-gate".into()),
            h.diff_hash(),
            Verdict::Go,
            ReviewDepth::FullDiff,
            None,
        );
        let err = h.cast(0, forged).unwrap_err();
        assert_eq!(err, CastRejected::BadSignature);
    }

    fn nogo(seed: u8, h: &DualHold, remedy: Remedy) -> CastVerdict {
        let signer = Ed25519Signer::from_seed([seed; 32]);
        let clock = FixedClock(2000 + seed as i64);
        CastVerdict::create(
            &signer,
            &clock,
            h.gate_id(),
            h.diff_hash(),
            Verdict::NoGo(remedy),
            ReviewDepth::FullDiff,
            None,
        )
    }

    #[test]
    fn nogo_blocks_and_retains_remedy() {
        let mut h = hold();
        h.cast(0, go(1, &h)).unwrap();
        let steer = Remedy::Steer("cache the lookup".into());
        let out = h.cast(1, nogo(2, &h, steer.clone())).unwrap();
        assert_eq!(out.state, HoldState::Blocked);
        assert_eq!(h.blocking_remedy(), Some(steer));
        assert_eq!(h.outcome(), None); // blocked is not a satisfied outcome
    }

    #[test]
    fn handedit_strict_two_operators_signals_escalation() {
        let a = Ed25519Signer::from_seed([1; 32]).operator_id();
        let b = Ed25519Signer::from_seed([2; 32]).operator_id();
        // A hand-edits; strict mode; only A and B exist → eligible pool = {B} = 1 < 2.
        let mut h = DualHold::reopen_handedit(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            a,
            true,
            &[a, b],
        );
        assert_eq!(h.authorship(), Authorship::Both);
        // B can cast, but the outcome flags escalation because two eligible
        // keys are unreachable; A (the editor) is ineligible.
        let out = h.cast(0, go(2, &h)).unwrap();
        assert!(out.escalation_required);
        assert!(matches!(
            h.cast(1, go(1, &h)).unwrap_err(),
            CastRejected::Ineligible
        ));
    }

    #[test]
    fn handedit_pragmatic_editor_may_cosign() {
        let a = Ed25519Signer::from_seed([1; 32]).operator_id();
        let b = Ed25519Signer::from_seed([2; 32]).operator_id();
        let policy = GatePolicy {
            independence: crate::Independence::Pragmatic,
            ..GatePolicy::default()
        };
        let mut h = DualHold::reopen_handedit(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            policy,
            MakerSet::new(),
            a,
            true,
            &[a, b],
        );
        // Editor A co-signs (allowed in pragmatic), B co-signs → satisfied.
        let out = h.cast(0, go(1, &h)).unwrap();
        assert!(!out.escalation_required);
        let out = h.cast(1, go(2, &h)).unwrap();
        assert_eq!(out.state, HoldState::Satisfied);
        assert_eq!(h.outcome(), Some(Outcome::ResolvedAfterDisagreement));
    }

    #[test]
    fn reopened_hold_records_resolved_after_disagreement() {
        let mut h = DualHold::reopen(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Both,
        );
        h.cast(0, go(1, &h)).unwrap();
        h.cast(1, go(2, &h)).unwrap();
        assert_eq!(h.state(), HoldState::Satisfied);
        assert_eq!(h.outcome(), Some(Outcome::ResolvedAfterDisagreement));
    }
}

#[cfg(test)]
mod prop {
    use super::tests_support::*;
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // Invariant #1 & #7: SATISFIED requires exactly two distinct GO keys;
        // one key alone never satisfies.
        #[test]
        fn never_satisfies_on_a_single_key(seed in 0u8..64) {
            let mut h = fresh_hold();
            let out = h.cast(0, go_for(seed, &h)).unwrap();
            prop_assert_eq!(out.state, HoldState::Partial);
            prop_assert!(h.outcome().is_none());
        }

        // Invariant #3: while blind + partial, no verdict value is observable.
        #[test]
        fn sealed_value_never_leaks_while_partial(seed in 0u8..64) {
            let mut h = fresh_hold();
            h.cast(0, go_for(seed, &h)).unwrap();
            for view in h.observed_verdicts() {
                prop_assert_eq!(view.status, crate::VerdictStatus::Sealed);
            }
        }
    }
}

// Shared constructors for the property module (kept out of the value-test
// module to avoid `use super::tests::…` visibility gymnastics).
#[cfg(test)]
mod tests_support {
    use super::*;
    use crate::ids::Hash;
    use crate::sign::{Ed25519Signer, FixedClock};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth, Verdict};

    pub fn fresh_hold() -> DualHold {
        DualHold::new(
            GateId("g1".into()),
            TaskId("t1".into()),
            Hash([9u8; 32]),
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Agent,
        )
    }

    pub fn go_for(seed: u8, h: &DualHold) -> CastVerdict {
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
}
