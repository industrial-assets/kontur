# Kontur Core Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `kontur-core`, a pure headless Rust library implementing the four-eyes dual-hold gate and the hash-chained signed audit record — the invariant-bearing heart of Kontur.

**Architecture:** A single library crate in a Cargo workspace. All logic is a deterministic, synchronous state machine with zero I/O; time, signing, and persistence are injected via traits so every test is reproducible. Illegal states (a bare veto, a leaked sealed verdict, a single-key merge) are made unrepresentable or rejected at cast time, never merely at display.

**Tech Stack:** Rust (stable, 1.93+), `ed25519-dalek` (signing), `sha2` (hashing), `ciborium` (deterministic CBOR for canonical bytes), `serde`, `thiserror`, `proptest` (dev).

## Global Constraints

- **Rust edition 2021**, toolchain `stable` (verified 1.93.1 available).
- **No wall-clock, no RNG inside `kontur-core`.** Time via the `Clock` trait; signing keys constructed from explicit seed bytes. This is required for deterministic audit reproducibility and tests.
- **No `HashMap`/`HashSet` in any type that is serialized for hashing or signing** — use ordered structs/`Vec` so canonical bytes are stable.
- **Crypto is load-bearing (CLAUDE.md):** never log a secret, never weaken signature generation/verification, never make an audit record mutable after emission.
- **Invariants (CLAUDE.md) enforced structurally:** two distinct keys (#1), eligibility at cast time (#2), blind sealing (#3), no bare veto by type (#4), hand-edit = fresh deferred hold (#5), tamper-evident chain (#6), fail-safe never degrades to one key (#7).
- **Policy defaults:** Independence `Strict`, Blind `on`, Availability `Park`.
- **Commit after every task.** Co-author trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

## File structure

```
kontur/
  Cargo.toml                    # [workspace] members = ["crates/kontur-core"]
  crates/
    kontur-core/
      Cargo.toml
      src/
        lib.rs                  # module wiring + public re-exports
        ids.rs                  # OperatorId, GateId, TaskId, Timestamp, Hash, Sig, HandEditRef
        verdict.rs              # Verdict, Remedy, ReviewDepth, CastVerdict, SignedContent
        canonical.rs            # canonical_bytes() + sha256()
        sign.rs                 # Signer, Clock traits; Ed25519Signer; verify(); fakes
        policy.rs               # GatePolicy, Independence, Availability, Authorship, Outcome
        eligibility.rs          # MakerSet + eligibility check
        sealed.rs               # SealedVerdict + VerdictView
        hold.rs                 # DualHold, HoldState, cast(), HoldOutcome, CastRejected
        audit/
          mod.rs
          record.rs             # GateRecord, Provenance, CheckerEntry, build_record()
          chain.rs              # AuditChain, append(), verify_chain(), reviewed_by()
      tests/
        integration.rs          # UX §7 narrative paths + determinism
```

Each task below lists exact files and complete code. Tasks are ordered by dependency; each ends with a green test and a commit.

---

### Task 1: Workspace scaffold + core id/verdict types

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/kontur-core/Cargo.toml`
- Create: `crates/kontur-core/src/lib.rs`
- Create: `crates/kontur-core/src/ids.rs`
- Create: `crates/kontur-core/src/verdict.rs`

**Interfaces:**
- Produces: `OperatorId([u8;32])`, `GateId(String)`, `TaskId(String)`, `HandEditRef(String)`, `Timestamp(i64)`, `Hash([u8;32])`, `Sig([u8;64])`; `Verdict::{Go, NoGo(Remedy)}`, `Remedy::{Steer(String), HandEdit(HandEditRef)}`, `ReviewDepth::{FullDiff, Summary, TestsRun}`.

- [ ] **Step 1: Create the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/kontur-core"]
```

- [ ] **Step 2: Create `crates/kontur-core/Cargo.toml`**

```toml
[package]
name = "kontur-core"
version = "0.1.0"
edition = "2021"

[dependencies]
ed25519-dalek = { version = "2", features = ["rand_core"] }
sha2 = "0.10"
ciborium = "0.2"
serde = { version = "1", features = ["derive"] }
serde-big-array = "0.5"
thiserror = "2"

[dev-dependencies]
proptest = "1"
```

- [ ] **Step 3: Create `crates/kontur-core/src/lib.rs`**

```rust
//! Kontur core: the four-eyes dual-hold gate and tamper-evident audit chain.
//!
//! Pure, synchronous, no I/O. Time and signing are injected via traits.

pub mod ids;
pub mod verdict;

pub use ids::{GateId, HandEditRef, Hash, OperatorId, Sig, TaskId, Timestamp};
pub use verdict::{Remedy, ReviewDepth, Verdict};
```

- [ ] **Step 4: Create `crates/kontur-core/src/ids.rs`**

```rust
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

/// An operator's stable identity: their Ed25519 public key bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct OperatorId(pub [u8; 32]);

/// Identifier for a gate (one per gated action).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GateId(pub String);

/// Identifier for a task in the plan.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TaskId(pub String);

/// Reference to a direct human change (a hand-edit).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct HandEditRef(pub String);

/// Milliseconds since the Unix epoch. Supplied by the injected `Clock`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

/// A 32-byte SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

/// A 64-byte Ed25519 signature. `serde` only derives arrays up to length 32, so
/// the 64-byte field uses `serde-big-array`. `[u8; 64]` implements
/// `PartialEq`/`Eq`/`Debug`/`Copy` for all N in std, so those still derive.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Sig(#[serde(with = "BigArray")] pub [u8; 64]);
```

- [ ] **Step 5: Create `crates/kontur-core/src/verdict.rs`**

```rust
use serde::{Deserialize, Serialize};

use crate::ids::HandEditRef;

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
```

- [ ] **Step 6: Write the failing test** — append to `crates/kontur-core/src/verdict.rs`

```rust
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
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p kontur-core`
Expected: PASS (2 tests). This also confirms the workspace and deps compile.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/kontur-core
git commit -m "feat(core): scaffold workspace + verdict/id types

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Canonical serialization + hashing

**Files:**
- Create: `crates/kontur-core/src/canonical.rs`
- Modify: `crates/kontur-core/src/lib.rs` (add `pub mod canonical;` and re-exports)

**Interfaces:**
- Consumes: nothing new.
- Produces: `canonical_bytes<T: Serialize>(&T) -> Vec<u8>`; `sha256(&[u8]) -> Hash`.

- [ ] **Step 1: Write the failing test** — create `crates/kontur-core/src/canonical.rs`

```rust
use crate::ids::Hash;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Deterministic CBOR encoding. Structs encode their fields in declaration
/// order, so identical values always produce identical bytes — the basis of a
/// reproducible audit hash. Never feed a `HashMap`/`HashSet` through this.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).expect("serialization is infallible for our types");
    buf
}

/// SHA-256 of an arbitrary byte slice.
pub fn sha256(bytes: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    Hash(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn canonical_bytes_are_stable_across_calls() {
        let s = Sample { a: 7, b: "hi".into() };
        assert_eq!(canonical_bytes(&s), canonical_bytes(&s));
    }

    #[test]
    fn different_values_differ() {
        let s1 = Sample { a: 7, b: "hi".into() };
        let s2 = Sample { a: 8, b: "hi".into() };
        assert_ne!(canonical_bytes(&s1), canonical_bytes(&s2));
    }

    #[test]
    fn sha256_is_deterministic_and_sensitive() {
        assert_eq!(sha256(b"abc"), sha256(b"abc"));
        assert_ne!(sha256(b"abc"), sha256(b"abd"));
    }
}
```

- [ ] **Step 2: Wire the module** — edit `crates/kontur-core/src/lib.rs`, add after `pub mod canonical;` line grouping:

```rust
pub mod canonical;
pub mod ids;
pub mod verdict;

pub use canonical::{canonical_bytes, sha256};
pub use ids::{GateId, HandEditRef, Hash, OperatorId, Sig, TaskId, Timestamp};
pub use verdict::{Remedy, ReviewDepth, Verdict};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core canonical`
Expected: PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): canonical CBOR bytes + sha256

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Signing & clock traits + Ed25519 signer + verify

**Files:**
- Create: `crates/kontur-core/src/sign.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: `OperatorId`, `Sig`, `Timestamp` (Task 1).
- Produces:
  - `trait Signer { fn operator_id(&self) -> OperatorId; fn sign(&self, msg: &[u8]) -> Sig; }`
  - `trait Clock { fn now(&self) -> Timestamp; }`
  - `struct Ed25519Signer` with `Ed25519Signer::from_seed([u8;32]) -> Self` implementing `Signer`.
  - `fn verify(op: OperatorId, msg: &[u8], sig: &Sig) -> bool`.
  - Test fakes `FixedClock`.

- [ ] **Step 1: Write the implementation + failing test** — create `crates/kontur-core/src/sign.rs`

```rust
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};

use crate::ids::{OperatorId, Sig, Timestamp};

/// Produces signed verdicts. In production an operator's key lives on their
/// station; in tests we construct one from a fixed seed for determinism.
pub trait Signer {
    fn operator_id(&self) -> OperatorId;
    fn sign(&self, msg: &[u8]) -> Sig;
}

/// Injected time source — the core never reads the wall clock.
pub trait Clock {
    fn now(&self) -> Timestamp;
}

/// Ed25519 signer built from a 32-byte seed (deterministic; no RNG).
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Ed25519Signer {
            key: SigningKey::from_bytes(&seed),
        }
    }
}

impl Signer for Ed25519Signer {
    fn operator_id(&self) -> OperatorId {
        OperatorId(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, msg: &[u8]) -> Sig {
        Sig(self.key.sign(msg).to_bytes())
    }
}

/// Verify a signature against the public key embedded in `op`. Returns false on
/// any malformed key/signature — never panics.
pub fn verify(op: OperatorId, msg: &[u8], sig: &Sig) -> bool {
    let vk = match VerifyingKey::from_bytes(&op.0) {
        Ok(vk) => vk,
        Err(_) => return false,
    };
    let signature = ed25519_dalek::Signature::from_bytes(&sig.0);
    vk.verify(msg, &signature).is_ok()
}

/// A clock that always returns the same instant — for deterministic tests.
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let signer = Ed25519Signer::from_seed([1u8; 32]);
        let op = signer.operator_id();
        let msg = b"gate-03 diff-hash go";
        let sig = signer.sign(msg);
        assert!(verify(op, msg, &sig));
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let signer = Ed25519Signer::from_seed([1u8; 32]);
        let op = signer.operator_id();
        let sig = signer.sign(b"original");
        assert!(!verify(op, b"tampered", &sig));
    }

    #[test]
    fn verify_rejects_wrong_operator() {
        let a = Ed25519Signer::from_seed([1u8; 32]);
        let b = Ed25519Signer::from_seed([2u8; 32]);
        let msg = b"msg";
        let sig = a.sign(msg);
        assert!(!verify(b.operator_id(), msg, &sig));
    }

    #[test]
    fn distinct_seeds_give_distinct_identities() {
        let a = Ed25519Signer::from_seed([1u8; 32]);
        let b = Ed25519Signer::from_seed([2u8; 32]);
        assert_ne!(a.operator_id(), b.operator_id());
    }
}
```

- [ ] **Step 2: Wire the module** — edit `crates/kontur-core/src/lib.rs`:

```rust
pub mod canonical;
pub mod ids;
pub mod sign;
pub mod verdict;

pub use canonical::{canonical_bytes, sha256};
pub use ids::{GateId, HandEditRef, Hash, OperatorId, Sig, TaskId, Timestamp};
pub use sign::{verify, Clock, Ed25519Signer, Signer};
pub use verdict::{Remedy, ReviewDepth, Verdict};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core sign`
Expected: PASS (4 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): Signer/Clock traits + Ed25519 signer + verify

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Policy types + defaults

**Files:**
- Create: `crates/kontur-core/src/policy.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Produces: `Independence::{Strict, Pragmatic}`, `Availability::{Park, EscalateAfter(u64)}`, `Authorship::{Agent, HandEdited, Both}`, `Outcome::{Unanimous, ResolvedAfterDisagreement}`, `GatePolicy { required: u8, independence, blind, availability }`, `GatePolicy::default()`.

- [ ] **Step 1: Write the implementation + failing test** — create `crates/kontur-core/src/policy.rs`

```rust
use serde::{Deserialize, Serialize};

/// Whether the change's maker may also be a checker.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Independence {
    /// The maker (prompt author / hand-editor) may not cast a counting verdict.
    Strict,
    /// The maker may be one of the two, but the co-signer must be a non-maker.
    Pragmatic,
}

/// What happens when two eligible keys cannot be gathered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Availability {
    /// Hold parks indefinitely (safe default) — never degrade to one key.
    Park,
    /// After this many milliseconds, signal escalation to a third signatory.
    EscalateAfter(u64),
}

/// Provenance of the change under review.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Authorship {
    Agent,
    HandEdited,
    Both,
}

/// How a satisfied gate resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Outcome {
    Unanimous,
    ResolvedAfterDisagreement,
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
```

- [ ] **Step 2: Wire the module** — edit `crates/kontur-core/src/lib.rs`, add `pub mod policy;` (keep modules alphabetical) and:

```rust
pub use policy::{Authorship, Availability, GatePolicy, Independence, Outcome};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core policy`
Expected: PASS (1 test).

- [ ] **Step 4: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): gate policy types with strict/blind/park defaults

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Maker-checker eligibility

**Files:**
- Create: `crates/kontur-core/src/eligibility.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: `OperatorId` (Task 1), `Independence` (Task 4).
- Produces: `MakerSet` with `MakerSet::new()`, `MakerSet::with(OperatorId)`, `MakerSet::contains(&OperatorId) -> bool`; `fn is_eligible(independence, makers: &MakerSet, op: OperatorId) -> bool`.

- [ ] **Step 1: Write the implementation + failing test** — create `crates/kontur-core/src/eligibility.rs`

```rust
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
```

- [ ] **Step 2: Wire the module** — edit `crates/kontur-core/src/lib.rs`, add `pub mod eligibility;` and:

```rust
pub use eligibility::{is_eligible, MakerSet};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core eligibility`
Expected: PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): maker-checker eligibility (strict/pragmatic)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: CastVerdict, SignedContent, and sealed verdicts

**Files:**
- Modify: `crates/kontur-core/src/verdict.rs` (add `CastVerdict`, `SignedContent`, `sign_content`)
- Create: `crates/kontur-core/src/sealed.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: `Verdict`, `ReviewDepth` (Task 1), `OperatorId`, `GateId`, `Hash`, `Sig`, `Timestamp` (Task 1), `Signer`, `verify` (Task 3), `canonical_bytes` (Task 2).
- Produces:
  - `SignedContent { gate_id, diff_hash, operator, verdict, depth, cast_at }` (the bytes that get signed).
  - `CastVerdict { operator, verdict, depth, comment: Option<String>, cast_at, signature }` with `CastVerdict::create(signer, clock, gate_id, diff_hash, verdict, depth, comment) -> Self` and `verify_signature(&self, gate_id, diff_hash) -> bool`.
  - `SealedVerdict` wrapping a `CastVerdict` with a `sealed` flag; `operator()`, `reveal() -> Option<&CastVerdict>`, `unseal()`, and crate-internal `raw()`.
  - `VerdictView { operator, status: VerdictStatus }`, `VerdictStatus::{Sealed, Revealed(Verdict)}`.

- [ ] **Step 1: Extend `crates/kontur-core/src/verdict.rs`** — add below the existing types (before the `#[cfg(test)]` module):

```rust
use crate::canonical::canonical_bytes;
use crate::ids::{GateId, Hash, OperatorId, Sig, Timestamp};
use crate::sign::{verify, Clock, Signer};

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
```

- [ ] **Step 2: Create `crates/kontur-core/src/sealed.rs`**

```rust
use crate::ids::OperatorId;
use crate::verdict::{CastVerdict, Verdict};

/// A cast verdict whose value is hidden while `sealed` is true (blind review,
/// invariant #3). The operator identity is always visible (needed for
/// deduplication and eligibility); the *verdict* is not.
#[derive(Clone, Debug)]
pub struct SealedVerdict {
    cv: CastVerdict,
    sealed: bool,
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
```

- [ ] **Step 3: Wire modules & re-exports** — edit `crates/kontur-core/src/lib.rs`, add `pub mod sealed;`, and:

```rust
pub use sealed::{SealedVerdict, VerdictStatus, VerdictView};
pub use verdict::{CastVerdict, Remedy, ReviewDepth, SignedContent, Verdict};
```

- [ ] **Step 4: Write the failing test** — append to `crates/kontur-core/src/sealed.rs`

```rust
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
    fn signature_roundtrips_and_binds_to_gate() {
        let cv = a_cast();
        assert!(cv.verify_signature(&GateId("g1".into()), Hash([0u8; 32])));
        // Replaying onto a different gate fails.
        assert!(!cv.verify_signature(&GateId("g2".into()), Hash([0u8; 32])));
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kontur-core`
Expected: PASS (all prior tests + 3 new).

- [ ] **Step 6: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): signed CastVerdict + sealed verdict projection

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: DualHold — happy path (OPEN → PARTIAL → SATISFIED)

**Files:**
- Create: `crates/kontur-core/src/hold.rs`
- Create: `crates/kontur-core/src/error.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces:
  - `HoldState::{Open, Partial, Satisfied, Blocked}`.
  - `CastRejected::{StaleVersion, DuplicateIdentity, Ineligible, AlreadyResolved, BadSignature}` (thiserror).
  - `HoldOutcome { state: HoldState, escalation_required: bool }`.
  - `DualHold` with:
    - `DualHold::new(gate_id, task_id, diff_hash, policy, makers, authorship) -> Self`
    - `DualHold::reopen(...) -> Self` (contested = true) — used in Task 10, defined here.
    - `fn state(&self) -> HoldState`
    - `fn version(&self) -> u64`
    - `fn cast(&mut self, expected_version, cv: CastVerdict) -> Result<HoldOutcome, CastRejected>`
    - `fn observed_verdicts(&self) -> Vec<VerdictView>`
    - `fn outcome(&self) -> Option<Outcome>` (Some once satisfied)

- [ ] **Step 1: Create `crates/kontur-core/src/error.rs`**

```rust
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
```

- [ ] **Step 2: Create `crates/kontur-core/src/hold.rs`**

```rust
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
    pub(crate) fn raw_verdicts(&self) -> &[SealedVerdict] {
        &self.verdicts
    }

    pub(crate) fn makers(&self) -> &MakerSet {
        &self.makers
    }

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
```

- [ ] **Step 3: Wire modules & re-exports** — edit `crates/kontur-core/src/lib.rs`, add `pub mod error;` and `pub mod hold;`, and:

```rust
pub use error::CastRejected;
pub use hold::{DualHold, HoldOutcome, HoldState};
```

- [ ] **Step 4: Write the failing test** — append to `crates/kontur-core/src/hold.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Hash;
    use crate::sign::{Ed25519Signer, FixedClock, Signer};
    use crate::verdict::CastVerdict;
    use crate::{GatePolicy, ReviewDepth, Verdict};

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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kontur-core hold`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): dual-hold happy path with blind sealing

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: DualHold — rejection paths

**Files:**
- Modify: `crates/kontur-core/src/hold.rs` (tests only — logic already present)

**Interfaces:**
- Consumes: `DualHold`, `CastRejected` (Task 7).
- Produces: no new API; proves `StaleVersion`, `DuplicateIdentity`, `Ineligible`, `AlreadyResolved`, `BadSignature`.

- [ ] **Step 1: Write the failing tests** — add to the `tests` module in `crates/kontur-core/src/hold.rs`

```rust
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
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p kontur-core hold`
Expected: PASS (7 tests total in the module).

- [ ] **Step 3: Commit**

```bash
git add crates/kontur-core/src
git commit -m "test(core): dual-hold rejection paths

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: DualHold — no-go / blocked path + property tests

**Files:**
- Modify: `crates/kontur-core/src/hold.rs` (add helpers + property tests)

**Interfaces:**
- Consumes: `DualHold`, `Verdict::NoGo`, `Remedy`, `Outcome` (prior tasks).
- Produces: proves blocked transitions, remedy retention, `ResolvedAfterDisagreement`, and adds `blocking_remedy()` helper.

- [ ] **Step 1: Add a helper to read the blocking remedy** — in `crates/kontur-core/src/hold.rs`, inside `impl DualHold` (before the closing brace of the impl):

```rust
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
```

- [ ] **Step 2: Write the failing tests** — add to the `tests` module in `crates/kontur-core/src/hold.rs`

```rust
    use crate::Remedy;

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
```

- [ ] **Step 3: Add property tests** — append a new module at the end of `crates/kontur-core/src/hold.rs`

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kontur-core hold`
Expected: PASS (value tests + 2 proptest cases).

- [ ] **Step 5: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): no-go blocks with remedy + invariant property tests

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Hand-edit fresh hold + escalation signal

**Files:**
- Modify: `crates/kontur-core/src/hold.rs` (refine `escalation_required`, add `eligible_pool` input)

**Interfaces:**
- Consumes: `DualHold::reopen`, `MakerSet`, `Independence` (prior).
- Produces: `DualHold::reopen_handedit(..., known_operators: &[OperatorId]) -> Self` which sets authorship and computes whether the eligible pool is too small; `HoldOutcome.escalation_required` becomes meaningful.

- [ ] **Step 1: Replace `escalation_required` and add the hand-edit constructor** — in `crates/kontur-core/src/hold.rs`.

First, add a field to `DualHold` (in the struct definition, after `contested: bool,`):

```rust
    eligible_pool: usize,
```

In `DualHold::new`, initialise it to `usize::MAX` (unknown pool ⇒ never forces escalation for agent-authored gates):

```rust
            contested: false,
            eligible_pool: usize::MAX,
            outcome: None,
```

(Apply the same `eligible_pool: usize::MAX,` line in the struct literal inside `reopen` is not needed — `reopen` delegates to `new`.)

Then replace the `escalation_required` method body:

```rust
    /// Strict independence with fewer eligible operators than required keys
    /// cannot clear — the caller must escalate (invariant #7). Signal only.
    fn escalation_required(&self) -> bool {
        self.eligible_pool < self.policy.required as usize
    }
```

Add the hand-edit constructor inside `impl DualHold`:

```rust
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
```

- [ ] **Step 2: Write the failing tests** — add to the `tests` module in `crates/kontur-core/src/hold.rs`

```rust
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
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core hold`
Expected: PASS (all hold tests including the 2 new).

- [ ] **Step 4: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): hand-edit fresh hold + escalation signal

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 11: GateRecord + build from a resolved hold

**Files:**
- Create: `crates/kontur-core/src/audit/mod.rs`
- Create: `crates/kontur-core/src/audit/record.rs`
- Modify: `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: `DualHold` (its crate-internal `raw_verdicts`, `contested`, ids), `Outcome`, `Authorship`, `Hash`, `Sig`, `OperatorId`, `Timestamp`, `ReviewDepth`, `Verdict`, `canonical_bytes`, `sha256`.
- Produces:
  - `Provenance { task_id, prompt, prompt_author, agent_id, agent_model, agent_version, diff_hash, files, loc, tokens }` (caller-supplied inputs).
  - `CheckerEntry { operator, cast_at, verdict, depth, comment, signature }`.
  - `RecordCore { prev_hash, gate_id, provenance, authorship, checkers, outcome }`.
  - `GateRecord { core: RecordCore, this_hash: Hash }` with `GateRecord::build(prev_hash, provenance, hold) -> Result<GateRecord, RecordError>` and `GateRecord::recompute_hash(&self) -> Hash`.
  - `RecordError::HoldNotSatisfied`.

- [ ] **Step 1: Create `crates/kontur-core/src/audit/mod.rs`**

```rust
pub mod record;

pub use record::{CheckerEntry, GateRecord, Provenance, RecordCore, RecordError};
```

- [ ] **Step 2: Create `crates/kontur-core/src/audit/record.rs`**

```rust
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
```

- [ ] **Step 3: Expose `raw_verdicts` to the audit module** — it is already `pub(crate)` in `hold.rs` (Task 7), so `audit::record` can call it. No change needed. Wire modules in `crates/kontur-core/src/lib.rs`, add `pub mod audit;` and:

```rust
pub use audit::{CheckerEntry, GateRecord, Provenance, RecordCore, RecordError};
```

- [ ] **Step 4: Write the failing test** — append to `crates/kontur-core/src/audit/record.rs`

```rust
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
        let mut h = satisfied_hold();
        // A fresh, open hold instead.
        h = DualHold::new(
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kontur-core audit`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): gate record built from a satisfied hold

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 12: Audit chain — append, verify_chain, reviewed_by

**Files:**
- Create: `crates/kontur-core/src/audit/chain.rs`
- Modify: `crates/kontur-core/src/audit/mod.rs`, `crates/kontur-core/src/lib.rs`

**Interfaces:**
- Consumes: `GateRecord`, `Hash`, `OperatorId`, `verify`, `canonical_bytes`, `SignedContent`.
- Produces:
  - `AuditChain` with `AuditChain::new() -> Self` (genesis), `head() -> Hash`, `append(record) -> Result<(), ChainError>`, `records() -> &[GateRecord]`.
  - `fn verify_chain(records: &[GateRecord]) -> Result<(), ChainBreak>`.
  - `fn reviewed_by(record: &GateRecord) -> Vec<OperatorId>` (verified go signers, for the `Reviewed-by:` trailers).
  - `ChainBreak::{HashMismatch(usize), BrokenLink(usize), BadCheckerSignature(usize)}`, `ChainError::WrongPrevHash`.
- **Genesis hash constant:** `GENESIS: Hash = Hash([0u8;32])`.

- [ ] **Step 1: Create `crates/kontur-core/src/audit/chain.rs`**

```rust
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
```

- [ ] **Step 2: Wire modules** — edit `crates/kontur-core/src/audit/mod.rs`:

```rust
pub mod chain;
pub mod record;

pub use chain::{reviewed_by, verify_chain, AuditChain, ChainBreak, ChainError, GENESIS};
pub use record::{CheckerEntry, GateRecord, Provenance, RecordCore, RecordError};
```

Edit `crates/kontur-core/src/lib.rs`:

```rust
pub use audit::{
    reviewed_by, verify_chain, AuditChain, ChainBreak, ChainError, CheckerEntry, GateRecord,
    Provenance, RecordCore, RecordError, GENESIS,
};
```

- [ ] **Step 3: Write the failing tests** — append to `crates/kontur-core/src/audit/chain.rs`

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kontur-core audit::chain`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/kontur-core/src
git commit -m "feat(core): audit chain append/verify + reviewed-by trailers

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 13: Integration tests — UX §7 narratives + determinism

**Files:**
- Create: `crates/kontur-core/tests/integration.rs`

**Interfaces:**
- Consumes: the public `kontur_core` API (all prior tasks).
- Produces: no new API; end-to-end coverage of the three UX §7 journeys and byte-level determinism.

- [ ] **Step 1: Write the failing tests** — create `crates/kontur-core/tests/integration.rs`

```rust
use kontur_core::{
    reviewed_by, verify_chain, AuditChain, Authorship, CastVerdict, DualHold, Ed25519Signer,
    FixedClock, GateId, GatePolicy, GateRecord, Hash, HoldState, Independence, MakerSet, Outcome,
    Provenance, Remedy, ReviewDepth, Signer, TaskId, Verdict, GENESIS,
};

fn provenance(diff: Hash, author: kontur_core::OperatorId) -> Provenance {
    Provenance {
        task_id: TaskId("t1".into()),
        prompt: "refactor session guard to token store".into(),
        prompt_author: author,
        agent_id: "agent-03".into(),
        agent_model: "claude-opus-4-8".into(),
        agent_version: "1.0".into(),
        diff_hash: diff,
        files: vec!["auth/session.ts".into()],
        loc: 59,
        tokens: 6400,
    }
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

// UX §7: "Clean task" — dispatch → both go → merge, calm throughout.
#[test]
fn clean_task_produces_a_verified_record() {
    let diff = Hash([9u8; 32]);
    let mut h = DualHold::new(
        GateId("g1".into()),
        TaskId("t1".into()),
        diff,
        GatePolicy::default(),
        MakerSet::new(),
        Authorship::Agent,
    );
    h.cast(0, go(1, &h)).unwrap();
    h.cast(1, go(2, &h)).unwrap();
    assert_eq!(h.state(), HoldState::Satisfied);

    let author = Ed25519Signer::from_seed([1; 32]).operator_id();
    let mut chain = AuditChain::new();
    let rec = GateRecord::build(chain.head(), provenance(diff, author), &h).unwrap();
    chain.append(rec).unwrap();

    assert!(verify_chain(chain.records()).is_ok());
    assert_eq!(rec_outcome(&chain), Outcome::Unanimous);
    assert_eq!(reviewed_by(&chain.records()[0]).len(), 2);
}

fn rec_outcome(chain: &AuditChain) -> Outcome {
    chain.records()[0].core.outcome
}

// UX §7: "Caught in review" — no-go with a steer, then a clean second pass on a
// re-opened (contested) hold → resolved-after-disagreement.
#[test]
fn caught_in_review_records_resolved_after_disagreement() {
    let diff = Hash([9u8; 32]);
    // First pass: navigator casts no-go with a steer.
    let mut first = DualHold::new(
        GateId("g1".into()),
        TaskId("t1".into()),
        diff,
        GatePolicy::default(),
        MakerSet::new(),
        Authorship::Agent,
    );
    first.cast(0, go(1, &first)).unwrap();
    let steer = Remedy::Steer("cache the token lookup".into());
    first.cast(1, nogo(2, &first, steer)).unwrap();
    assert_eq!(first.state(), HoldState::Blocked);
    assert!(first.blocking_remedy().is_some());

    // Agent reworks; second pass on a fresh contested hold over the new diff.
    let diff2 = Hash([10u8; 32]);
    let mut second = DualHold::reopen(
        GateId("g1".into()),
        TaskId("t1".into()),
        diff2,
        GatePolicy::default(),
        MakerSet::new(),
        Authorship::Agent,
    );
    second.cast(0, go(1, &second)).unwrap();
    second.cast(1, go(2, &second)).unwrap();
    assert_eq!(second.outcome(), Some(Outcome::ResolvedAfterDisagreement));
}

// UX §7: "Emergency" — hand-edit applied; pragmatic mode lets the editor
// co-sign; combined diff re-signed by both before merge; authorship flagged.
#[test]
fn emergency_handedit_pragmatic_merges_with_both_authorship() {
    let a = Ed25519Signer::from_seed([1; 32]).operator_id();
    let b = Ed25519Signer::from_seed([2; 32]).operator_id();
    let policy = GatePolicy {
        independence: Independence::Pragmatic,
        ..GatePolicy::default()
    };
    let mut h = DualHold::reopen_handedit(
        GateId("g1".into()),
        TaskId("t1".into()),
        Hash([11u8; 32]),
        policy,
        MakerSet::new(),
        a,
        true,
        &[a, b],
    );
    assert_eq!(h.authorship(), Authorship::Both);
    h.cast(0, go(1, &h)).unwrap(); // editor A co-signs (pragmatic)
    h.cast(1, go(2, &h)).unwrap();
    assert_eq!(h.state(), HoldState::Satisfied);
    assert_eq!(h.authorship(), Authorship::Both);
}

// Determinism: identical inputs (fixed clock + seeded keys) yield byte-identical
// record hashes across independent runs — audit reproducibility.
#[test]
fn records_are_deterministic() {
    fn build_once() -> Hash {
        let diff = Hash([9u8; 32]);
        let mut h = DualHold::new(
            GateId("g1".into()),
            TaskId("t1".into()),
            diff,
            GatePolicy::default(),
            MakerSet::new(),
            Authorship::Agent,
        );
        h.cast(0, go(1, &h)).unwrap();
        h.cast(1, go(2, &h)).unwrap();
        let author = Ed25519Signer::from_seed([1; 32]).operator_id();
        GateRecord::build(GENESIS, provenance(diff, author), &h)
            .unwrap()
            .this_hash
    }
    assert_eq!(build_once(), build_once());
}
```

- [ ] **Step 2: Confirm the public API re-exports needed by the test** — verify `crates/kontur-core/src/lib.rs` re-exports every name the test imports. It must expose: `reviewed_by, verify_chain, AuditChain, Authorship, CastVerdict, DualHold, Ed25519Signer, FixedClock, GateId, GatePolicy, GateRecord, Hash, HoldState, Independence, MakerSet, Outcome, OperatorId, Provenance, Remedy, ReviewDepth, Signer, TaskId, Verdict, GENESIS`. Add any missing to the `pub use` lines (notably `FixedClock` and `Signer` from `sign`, and `OperatorId` from `ids`):

```rust
pub use sign::{verify, Clock, Ed25519Signer, FixedClock, Signer};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p kontur-core --test integration`
Expected: PASS (4 tests).

- [ ] **Step 4: Run the whole suite + clippy**

Run: `cargo test -p kontur-core && cargo clippy -p kontur-core --all-targets -- -D warnings`
Expected: all tests PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/kontur-core
git commit -m "test(core): end-to-end UX narratives + determinism

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review notes (for the executor)

- **Spec coverage:** every §2 invariant maps to a task — #1/#7 (Tasks 7, 9, 10), #2 (Tasks 5, 8, 10), #3 (Tasks 6, 7, 9), #4 (Task 1), #5 (Task 10), #6 (Tasks 11, 12). Audit record §9 fields (Task 11). Policy defaults §6 (Task 4).
- **Deferred to later slices (not this plan):** MCP wiring, network/attach server, TUI, real escalation timer, git merge — per spec §1.
- **Serde arrays:** `[u8; 32]` (`Hash`, `OperatorId`) derive serde directly; the 64-byte `Sig` uses `serde-big-array`'s `BigArray` (Task 1). If a future type needs another >32 array, apply the same `#[serde(with = "BigArray")]` attribute.
- **Determinism depends on ordered types only:** no `HashMap`/`HashSet` is serialized anywhere in the record path (`RecordCore`, `Provenance`, `CheckerEntry` are all structs/`Vec`), so canonical bytes — and the chain hash — are reproducible.
```
