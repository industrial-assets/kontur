# Kontur MCP enforcement plane — design spec

**Date:** 2026-07-17 · **Owner:** John · **Status:** Approved for planning

The second slice of Kontur: **`kontur-mcp`**, the enforcement plane that turns the
headless four-eyes engine (`kontur-core`) into a running gate over real agent actions.
It hosts an MCP server the agent talks to, parks the consequential boundary action,
drives a `DualHold` to two independent sign-offs, and emits the tamper-evident audit
record — the machinery that lives "between MCP's pause and MCP's resume" (PRD §10.1).

**Stack (confirmed):** Rust + `rmcp` 2.2.0 (official Rust MCP SDK, tokio-based) + `tokio`.
`kontur-core` stays synchronous and pure; this crate bridges it into async.

---

## 1. Scope

### In scope (this slice)

1. **Gate host / orchestrator** — the glue between an intercepted boundary action and
   `kontur-core`: open a `DualHold`, collect two signed verdicts, on `SATISFIED`
   accept + emit the audit record, on `BLOCKED` discard + return the remedy.
2. **A real MCP server** (rmcp) exposing the agent-facing tools, where the gated tool
   handler *awaits its gate* before returning.
3. **A `Workspace` port** abstracting worktree operations, with a filesystem-backed
   implementation and a test double.
4. **Audit emission** — appending a `GateRecord` per satisfied gate to a `kontur-core`
   `AuditChain`, and exposing `reviewed_by` trailer data.

### Out of scope (own later slices)

- **The Claude Code binding** — forcing the agent's consequential actions through this
  server rather than its native file/shell tools (sandboxing / tool-restriction /
  hooks). This slice provides the gated boundary and recording; the *guarantee* that the
  agent cannot bypass it is the deferred binding.
- **The network / attach layer** — multi-client presence, claiming, state sync. The
  operator face is an in-process port here; the network slice implements it remotely.
- **The TUI.**
- **Real git** — branch/commit/merge and the session roll-up. Deferred behind
  `Workspace::accept_task`; this slice records acceptance and emits the audit record.
- **Escalation timers** — operator loss parks the gate (handler awaits); the
  `escalation_required` signal from the engine is surfaced but no timer runs.
- **Risk-tiering** — uniform `Strict/Blind/Park` policy; tiers are a PRD "later" item.

### Divergences from the PRD (flagged, not silently taken)

1. **Gate granularity.** We gate the *task-completion boundary*, not every write/shell
   (PRD §10 says "route file writes, shell, merge … and gate them"). Rationale: the
   engine models one `DualHold` per gated action and one `GateRecord` per gate, and the
   workflow (§6, §8) reviews a *task's diff* in an isolated worktree. Per-write holds
   would be noisy and mismatch the per-task audit record.
2. **Review forms are not MCP elicitation.** PRD §10.1 says MCP elicitation renders each
   operator's review form. But operators are TUI clients, not the agent's MCP client, so
   the operator-facing review is a **separate in-process port**, driven by the net/TUI
   slice — not MCP elicitation.
3. **write/shell are recorded, not held.** They execute in the isolated worktree and are
   appended to the task's tool-trail; the trust-boundary gate is task acceptance. The
   non-bypass guarantee depends on the deferred Claude Code binding.

---

## 2. Architecture

New workspace crate `kontur-mcp`, depending on `kontur-core`, `rmcp = "2.2"`,
`tokio` (rt-multi-thread + macros + sync), `serde`, `thiserror`.

The gate host presents **two faces**:

### Agent face — the MCP server (rmcp)

Tools the agent calls:

- `write_file { path, contents }` — apply a write to the agent's worktree via the
  `Workspace` port; append a tool-trail entry to the current task. **Not gated.**
- `run_command { command, cwd }` — run a command in the worktree via `Workspace`;
  append a tool-trail entry. **Not gated.**
- `propose_task_complete { task_id }` — the **gated boundary action**. The handler
  freezes the task diff, opens a gate, and **awaits its resolution** before returning.
  The open MCP request *is* the agent waiting at the gate (one task at a time).

Reads and non-consequential tools are out of scope; the full agent tool surface is the
Claude Code binding slice's concern.

### Operator face — an in-process port (`GateHost` methods)

The seam the network/TUI slice will drive; tests drive it directly.

- `pending_gates() -> Vec<GateView>` — gates awaiting review; **respects blind sealing**
  (a `GateView` never exposes a sealed verdict's value — it projects `kontur-core`'s
  `observed_verdicts`).
- `submit_verdict(gate_id, CastVerdict) -> Result<GateProgress, GateHostError>` —
  routes to `DualHold::cast` with the version guard.
- `hand_edit(task_id, edit, editor_id) -> Result<GateId, GateHostError>` — applies the
  edit to the worktree immediately, then opens a fresh hold via
  `DualHold::reopen_handedit`.
- `audit_chain() -> AuditChainView` / `reviewed_by(gate_id) -> Vec<OperatorId>`.

Operators sign verdicts client-side (Ed25519); the host holds **no private keys** — only
the registry of operator public `OperatorId`s for the session.

### Bridging sync core to async host

`kontur-core` is synchronous and single-writer. The gate host owns the mutable session
state behind an `Arc<Mutex<SessionState>>` (a `tokio::sync::Mutex`), making the host the
single writer that supplies `DualHold::cast`'s `expected_version`. Each open gate carries
a `tokio::sync::watch::Sender<HoldState>`; the awaiting `propose_task_complete` handler
holds the matching `Receiver` and wakes on every state change until the hold is terminal.

---

## 3. Data flow

### `propose_task_complete` lifecycle

1. **Freeze.** `Workspace::freeze_task_diff(task_id) -> FrozenDiff { bytes, files, loc }`.
   Compute `diff_hash = kontur_core::sha256(canonical_bytes(&frozen.bytes))`.
2. **Provenance.** Assemble `kontur_core::Provenance` from the `SessionContext` (prompt,
   `prompt_author`, agent id/model/version, tokens) plus this task's `files`, `loc`, and
   `diff_hash`. Note: `kontur-core`'s `Provenance` has **no tool-trail field** — the
   tool-trail (`write_file`/`run_command` entries) is recorded on `kontur-mcp`'s own task
   state for operator review and future use, and is *not* folded into the `GateRecord`
   in this slice. If it should become part of the signed record, that is a `kontur-core`
   change raised separately, not worked around here.
3. **Open gate.** `open_gate` builds a `DualHold::new(gate_id, task_id, diff_hash,
   policy, makers, Authorship::Agent)` with the session `GatePolicy` (default
   `Strict/Blind/Park`); registers a `watch` channel; returns the `GateId`. The handler
   awaits the channel.
4. **Verdicts.** Operators call `submit_verdict`. Each casts on the `DualHold` under the
   session lock (version guard). The second eligible verdict resolves the hold; the host
   publishes the new `HoldState` on the gate's `watch`.
5. **SATISFIED** → `Workspace::accept_task(task_id)` (records acceptance; real git
   deferred); build `GateRecord::build(chain.head(), provenance, &hold)`; append to the
   `AuditChain`. The handler returns success: the accepted task ref plus `reviewed_by`
   trailer data.
6. **BLOCKED** → `Workspace::discard_task(task_id)`; the handler returns a **structured
   tool error** carrying the remedy (`Remedy::Steer` text or hand-edit ref) so the agent
   reworks. Task state → INTERVENED.

### Hand-edit

`hand_edit(task_id, edit, editor_id)`:
1. `Workspace::apply_write(edit)` immediately (instant apply, PRD FR-16).
2. `DualHold::reopen_handedit(gate_id, task_id, combined_diff_hash, policy, prior_makers,
   editor_id, agent_authored=true, known_operators)` — authorship flagged
   `Both`/`HandEdited`, strict-mode editor excluded, escalation signalled when the
   eligible pool < 2. Returns the new `GateId`; it needs a fresh dual sign-off before the
   combined diff is acceptable (deferred acceptance).

### Availability

Operator loss ⇒ no second verdict ⇒ the gate parks; the handler simply keeps awaiting
(`Availability::Park`). A session-shutdown path cancels outstanding awaits cleanly (the
`watch` senders drop, waking handlers with a "session closing" outcome). Escalation
timers are a later slice; the engine's `escalation_required` flag is surfaced on
`GateProgress`/`GateView` but drives no automatic action here.

---

## 4. Components

```
crates/kontur-mcp/
  Cargo.toml
  src/
    lib.rs           # module wiring + public re-exports
    session.rs       # SessionId, SessionContext, operator registry, GatePolicy default
    workspace.rs     # Workspace trait; FsWorkspace (filesystem-backed) + InMemoryWorkspace (test double)
    provenance.rs    # assemble kontur_core::Provenance from session + invocation context
    gatehost.rs      # GateHost + SessionState: open_gate, submit_verdict, hand_edit, resolution watches, audit chain
    server.rs        # rmcp server: tool defs (write_file, run_command, propose_task_complete) -> GateHost
    error.rs         # GateHostError, ToolError (thiserror)
  tests/
    gatehost.rs      # orchestration tests (see §5)
    server_mcp.rs    # end-to-end via an in-process rmcp client
```

Each file has one responsibility: `workspace` owns side effects on the tree; `gatehost`
owns the four-eyes orchestration and audit; `server` owns only protocol wiring and
translation to/from `GateHost` calls; `session` and `provenance` are plain data
assembly. No file reaches into another's internals — `server` never touches a
`DualHold` directly, only `GateHost` methods.

---

## 5. Testing strategy

### Orchestration (`tests/gatehost.rs`, using `InMemoryWorkspace`)

- **Clean task:** open a gate, submit two distinct `go` verdicts → `SATISFIED`;
  `accept_task` was called exactly once; the `AuditChain` has one record and
  `verify_chain` passes; `reviewed_by` lists both operators.
- **Caught in review:** one `go`, one `no-go` with a steer → `BLOCKED`; `accept_task`
  **not** called; `discard_task` called; the remedy is returned to the caller; task
  state INTERVENED.
- **Hand-edit:** `hand_edit` applies the write immediately (observable in the workspace)
  and opens a fresh hold with authorship `Both`; under pragmatic policy the editor may
  co-sign and reach `SATISFIED`; under strict with two operators the editor is
  `Ineligible` and `escalation_required` is signalled.
- **Verdict rejections surface:** a stale/duplicate/ineligible verdict returns the
  corresponding `GateHostError` and does not resolve the gate.

### End-to-end MCP (`tests/server_mcp.rs`, using an in-process rmcp client)

- Start the rmcp server wired to a `GateHost` over an in-memory transport.
- Client calls `write_file` → the workspace double records the write and a tool-trail
  entry; the tool returns success without gating.
- Client calls `propose_task_complete` → the call **blocks**; concurrently, two
  `submit_verdict` calls on the operator face resolve the gate → the tool call returns
  success and the chain holds one verified record.
- A `BLOCKED` variant: `propose_task_complete` returns a structured error carrying the
  steer remedy.

### Determinism & hygiene

- Inject `kontur-core`'s `Clock` and use seeded `Ed25519Signer`s so records are
  reproducible; assert on **outcomes**, not timing (async).
- No `HashMap`/`HashSet` in anything fed to `canonical_bytes` (provenance/tool-trail
  summaries use ordered structs/`Vec`).
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, zero warnings.

---

## 6. Interfaces consumed from `kontur-core`

This slice is a pure consumer of the already-built, reviewed public API:

- `DualHold::{new, reopen_handedit, cast, state, version, outcome, observed_verdicts,
  gate_id, task_id, diff_hash, authorship, blocking_remedy}`, `HoldState`,
  `HoldOutcome`, `CastRejected`.
- `GatePolicy` (+ `Independence`, `Availability`, `Authorship`, `Outcome`),
  `MakerSet`, `is_eligible`.
- `CastVerdict::{create, verify_signature}`, `Verdict`, `Remedy`, `ReviewDepth`,
  `SealedVerdict`/`VerdictView` (for `pending_gates` projection).
- `Provenance`, `GateRecord::build`, `AuditChain`, `verify_chain`, `reviewed_by`,
  `GENESIS`.
- `Signer`, `Clock`, `Ed25519Signer`, `sha256`, `canonical_bytes`, `OperatorId`,
  `GateId`, `TaskId`, `Hash`.

The gate host adds nothing to `kontur-core`; if a genuine gap appears mid-build, it is
raised, not worked around (the core is high-risk, reviewed code).

---

## 7. Global constraints (carried into the plan)

- Rust edition 2021, stable toolchain.
- `kontur-core` stays sync/pure; async lives only in `kontur-mcp`.
- The gate host is the single writer of session state and always supplies
  `DualHold::cast`'s `expected_version` (no double-count, PRD §10.1 atomicity).
- Blind sealing holds across the operator face: `pending_gates`/`GateView` must never
  expose a sealed verdict value (invariant #3).
- No single-key acceptance: `accept_task` is reachable only from a `SATISFIED` hold
  (invariants #1/#7). Fail-safe under operator loss = park, never degrade.
- Operators sign client-side; the host stores only public `OperatorId`s — never a
  private key, never logs a key or a sealed verdict (security is load-bearing).
- The audit chain is append-only; never mutate an emitted record (invariant #6).
- `.gitignore` already covers `/target`; stage only intended files.
- Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
