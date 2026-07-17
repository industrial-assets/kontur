# Kontur TUI console — design spec (first slice)

**Date:** 2026-07-17 · **Owner:** John · **Status:** Approved for planning

The first person-facing slice of Kontur: **`kontur-tui`**, a runnable ratatui console
over a real local `kontur-mcp` `GateHost`. It renders the brutalist operator console and
lets an operator drive the four-eyes review-and-sign-off flow, watching the audit chain
grow. Read alongside `UX-kontur.md` (the console anatomy and screen states this
implements) and the two completed engine slices (`kontur-core`, `kontur-mcp`).

**Stack (confirmed):** Rust + `ratatui` 0.30 + `crossterm` 0.29, on `tokio` (the GateHost
operator face is async).

---

## 1. Scope

### In scope (this slice)

A runnable console covering the **review-and-sign-off core**, driven by a demo harness:

1. **Console shell** — the persistent chrome from UX §4: banner, status strip, stations,
   fleet/watch-floor, log, active region, command line.
2. **Watch-floor** (UX §6.4) — one bordered panel per agent; calm unless a human is needed.
3. **Merge-gate dual-key sign-off** (UX §6.5) with **blind sealing** (§5.4): the first key
   shows `cast — sealed`, its value never rendered until both are in.
4. **No-go + remedy intervention** (UX §6.6) — `[r]` demands a steer/edit; no bare veto.
5. **Session close / audit** (UX §6.9) — merged summary with `Reviewed-by` + chain verified.
6. **Demo harness** — a **mock/scripted fleet** feeding agent status and a **scripted second
   operator** (a demo Ed25519 keypair) that casts the second verdict, so the full
   `sealed → revealed → accept → audit` flow runs against the real engine.

### Small `kontur-mcp` operator-face additions (part of this slice)

Sourced from data the `GateHost` already holds — honest growth of the operator face, not a
workaround:

- Add `files: Vec<String>` and `loc: u32` to `GateView` (from the hold's `Provenance`) so the
  gate panel can show the change summary.
- Add `GateHost::gate_diff(&GateId) -> Option<Vec<u8>>` returning the frozen diff bytes, so an
  operator can open the actual change (honors FR-24's intent that approval means opening the
  diff, not a summary alone).

### Out of scope (own later increments)

- Prompt co-construction / dispatch gate (UX §6.2); plan/DAG review (§6.3); discuss threads
  (§6.8) — `[d]` is a visible-but-stubbed action here.
- Real two-client presence, claiming, rotation, and the operator-disconnect screen (§6.10) —
  these need the deferred network/attach layer.
- A full scrolling diff pager — this slice shows the summary and can display the raw diff
  bytes in a simple pane; a rich pager is later.
- **The true two-human guarantee.** This slice's second key is *scripted*, so it is a
  dev/demo console, not the production dual-control seat. The engine still enforces
  distinct-key + eligibility (the scripted key is a distinct identity), but two *real*
  humans at two clients arrive with the network slice. The console must not imply otherwise.

---

## 2. Non-negotiables this slice must honour

- **Blind sealing (invariant #3, FR-12, UX §5.4):** a sealed verdict's value must never be
  rendered. Enforced structurally: `GateCard` keys are built only from `GateView.observed`
  (`kontur-core`'s `VerdictView`, which is sealing-safe), so the raw verdict cannot enter the
  view model.
- **No bare veto (invariant #4, FR-13):** `[r]` opens a remedy entry; a verdict is submitted
  only with a steer (or a hand-edit). The engine also rejects a remedy-less `NoGo` by type.
- **No decorative telemetry (UX §2, brutalist):** every on-screen element is decision-relevant.
  No faked "tests ok", no host CPU, no confidence scores. The mock fleet's token/status values
  are clearly demo data, not invented precision.
- **Never render or log a private key or a sealed verdict value.**
- **Terminal is restored** on normal exit, error, and panic (raw mode + alternate screen are
  always torn down).

---

## 3. Architecture

New workspace crate `kontur-tui` (a library plus a `kontur` demo binary). Depends on
`kontur-mcp`, `kontur-core`, `ratatui = "0.30"`, `crossterm = "0.29"`, `tokio`.

### Components (one responsibility each)

```
crates/kontur-tui/
  Cargo.toml
  src/
    lib.rs            # module wiring + re-exports
    view.rs           # SessionView + sub-structs (pure data snapshot)
    viewmodel.rs      # build SessionView from GateHost + FleetSource
    fleet.rs          # FleetSource trait + MockFleet (scripted agent status)
    render.rs         # pure render(frame, &SessionView) -> ratatui widgets
    input.rs          # Action enum + key -> Action mapping
    app.rs            # async event loop; crossterm terminal setup/teardown
    demo.rs           # wire a real GateHost + MockFleet + scripted second operator
    bin/kontur.rs     # entry point: run the demo console
  tests/
    render.rs         # TestBackend golden-cell assertions
    flow.rs           # headless pending -> cast -> scripted-second -> accept
```

### `SessionView` (pure snapshot)

```
SessionView {
  banner: Banner,                 // КОНТУР-1 // session // version
  status: StatusStrip,            // link, four_eyes, fleet_count, needs_you, tokens
  stations: [Station; 2],         // role (Driver/Navigator) + activity, per operator
  fleet: Vec<AgentCard>,          // id, status line, tokens, needs_signoff flag
  log: Vec<LogLine>,              // real actions (verdicts, patches, ripple)
  active: ActiveRegion,
}

ActiveRegion = Idle
             | Gate(GateCard)
             | Intervention(InterventionCard)
             | SessionClosed(AuditSummary)

GateCard { gate_id, task, files: Vec<String>, loc: u32,
           keys: [KeyView; 2], escalation_required, diff_preview: Option<String> }
KeyView  { operator_label, role, status: KeyStatus }  // KeyStatus = Awaiting | Sealed | Go | NoGo
```

`KeyView.status` is derived from `GateView.observed` (`VerdictView`): `Sealed` stays
`Sealed`; a revealed `Verdict::Go`/`NoGo` maps to `Go`/`NoGo`; an operator with no entry is
`Awaiting`. There is no path from a sealed verdict's value into `KeyView`.

### Data flow

- **viewmodel** reads the `GateHost` operator face (`pending_gates`, `gate_diff`,
  `verify_audit`, `reviewed_by`) and a `FleetSource` (mock), producing a `SessionView`. It is
  a pure function of those inputs — fully testable without a terminal.
- **render** is a pure function `(&mut Frame, &SessionView)`; no I/O, no engine calls.
- **app** owns the async loop: `render` → `crossterm::event::poll(timeout)` → `read` → map to
  `Action` → apply (call `GateHost` operator face and, for the scripted second key, submit the
  demo operator's verdict) → rebuild `SessionView`. crossterm event polling is synchronous and
  non-blocking; GateHost calls are awaited.
- **demo** builds a real `GateHost` (in-memory workspace), a `MockFleet` that scripts a couple
  of agents proposing a task, and a scripted second operator, so `bin/kontur` launches a live
  console.

### Error handling

- Terminal setup/teardown via a guard that restores cooked mode + leaves the alternate screen
  on `Drop`, including on panic (a panic hook restores first, then re-panics).
- GateHost operator-face errors (`GateHostError`) surface as a transient status-line message,
  never a crash. A rejected verdict (e.g. duplicate/ineligible) shows its reason and leaves the
  gate unchanged.

---

## 4. Screens & interaction (mapping UX §6)

- **Idle / watch-floor (§6.1, §6.4):** stations + fleet panels; calm. Fleet panel for an agent
  needing sign-off is the one emphasised element.
- **Merge gate (§6.5):** the `GateCard` renders the two keys; the not-yet-cast operator sees
  the other as `cast — sealed`. `[g]` go, `[r]` no-go+remedy, `[e]` hand-edit, `[d]` discuss
  (stub), and an action to open the diff (`gate_diff`). On the second (scripted) key landing,
  both reveal and the view moves toward acceptance.
- **Intervention (§6.6):** `[r]` opens a remedy input; submitting sends the steer and the log
  shows the routing (`no-go … → rework`). No bare veto.
- **Session close (§6.9):** when all gates are satisfied, `AuditSummary` shows the reviewed-by
  identities, the gate count, and `chain verified` (from `verify_audit`).

Global keys always live: `?` help, `q`/quit (with terminal restore), `tab` (role hand-off —
label swap only in this slice, no network). Every on-screen action shows its key inline.

---

## 5. Testing strategy

- **Render golden tests (`tests/render.rs`)** with ratatui `TestBackend`: render fixtures and
  assert buffer cells — banner contains `КОНТУР-1`; a `GateCard` shows the file list + `+N`
  LOC; a sealed `KeyView` renders `sealed` and NOT `go`/`no-go`; `SessionClosed` shows
  `unanimous` and `chain verified`.
- **Viewmodel tests:** a `GateHost` with one pending gate + one sealed verdict → `SessionView`
  with `ActiveRegion::Gate` whose keys are `[Sealed, Awaiting]` (order per station).
- **Input-mapping tests:** each key → expected `Action`; unknown keys → no-op.
- **Headless flow test (`tests/flow.rs`):** build a real `GateHost`, open a gate, apply a `go`
  Action from station A, submit the scripted second `go`, rebuild the view, assert
  `ActiveRegion::SessionClosed` with `verify_audit().is_ok()` and two reviewers.
- **Determinism/hygiene:** no wall-clock/RNG in the view/render (fixtures inject values);
  `cargo clippy --all-targets -- -D warnings` clean; terminal-restore guard covered by a unit
  test on the guard type (not the live terminal).

---

## 6. Interfaces consumed / added

- **Consumed from `kontur-mcp`:** `GateHost::{pending_gates, submit_verdict, hand_edit,
  gate_outcome, verify_audit, reviewed_by, begin_task_gate, record_write}`, `GateView`,
  `GateProgress`, `GateFinal`, `SessionContext`, `InMemoryWorkspace`.
- **Consumed from `kontur-core`:** `VerdictView`, `VerdictStatus`, `Verdict`, `Remedy`,
  `CastVerdict`, `Ed25519Signer`, `Signer`, `OperatorId`, `GateId`, `TaskId`, `HoldState`.
- **Added to `kontur-mcp` (this slice):** `GateView.files`, `GateView.loc`,
  `GateHost::gate_diff(&GateId) -> Option<Vec<u8>>`. Sourced from the hold's `Provenance` and
  the `Workspace` frozen diff the host already has. These are minimal, reviewed additions; if
  a larger change is tempted, it is raised, not worked around.

---

## 7. Global constraints (carried into the plan)

- Rust edition 2021, stable toolchain.
- Blind sealing never violated in the view/render path (build keys from `VerdictView` only).
- No bare veto in the UI (`[r]` requires a remedy).
- No decorative telemetry / no faked signals (brutalist); identity flourish stays in the banner.
- Never render/log a private key or a sealed verdict value.
- Terminal always restored (normal, error, panic).
- The second key is scripted — the UI must not present this as a genuine second human.
- `.gitignore` covers `/target`; stage only intended files; no `git add -A`.
- `cargo clippy --all-targets -- -D warnings` clean; test output pristine.
- Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
