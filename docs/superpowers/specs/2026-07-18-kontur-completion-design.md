# Kontur completion — design spec (v0.1 product)

**Date:** 2026-07-18 · **Owner:** John · **Status:** Approved for planning (autonomous completion directive)

The slice that turns the three existing crates into a working product: **two real
engineers at two consoles, over the network, jointly gating an agent's changes into a
real git repository, with every resolved gate audited.** It closes the tracked
invariant-#6 gap (blocked gates unaudited), adds real git effects, the attach/sync
layer, the missing console interactions, and a real-agent MCP endpoint.

---

## 1. What "works" means (acceptance shape)

1. `kontur host --repo <path>` starts a session: an operator port (two consoles
   attach), an agent port (a real MCP client can connect), and optionally a scripted
   demo agent. The session walks the PRD §6 arc: dispatch-ready → plan review →
   task gates → session close.
2. `kontur join --addr <host:port> --seat B --seed <n>` attaches a console. Each
   console signs verdicts with its **own** key. Blind sealing holds across the wire.
3. A task gate clears only on two distinct signed `go`s; a `no-go` requires a typed
   steer (no bare veto) and routes the remedy back to the agent; a hand-edit applies
   immediately and opens a fresh gate; escalation is signalled under strict policy.
4. Accepted tasks are committed to a real **session branch** in the target repo; at
   session close the branch is squash-merged as **one reviewed commit carrying
   `Reviewed-by:` trailers** derived from the verified signatures (FR-21).
5. **Every resolved gate — Satisfied or Blocked — emits a signed, hash-chained audit
   record** (invariant #6 / FR-20 closed), and `verify_chain` passes at close.
6. Operator loss parks gates (invariant #7): the console shows the drop; reconnect
   resumes; nothing ever degrades to one key.

## 2. Changes by crate

### `kontur-core` (small, invariant-closing + wire enablement)

- **`Outcome::Blocked`** added. `GateRecord::build` now accepts a **resolved** hold:
  `Satisfied` → outcome from `hold.outcome()` (unchanged); `Blocked` →
  `Outcome::Blocked` — the dissenting checker entry already carries the
  `NoGo(Remedy)` verdict and its signature, so the remedy and dissent are in the
  chained record with zero new fields. `Open`/`Partial` still error;
  `RecordError::HoldNotSatisfied` renamed **`HoldUnresolved`** (message updated).
- **Serde on the sealing-safe projections**: derive `Serialize`/`Deserialize` on
  `VerdictStatus` and `VerdictView` so they can cross the wire. This is safe by
  construction — `Sealed` is a data-free variant; `SealedVerdict` itself remains
  non-serializable (the leak channel stays closed).

### `kontur-mcp`

- **Blocked-path audit emission**: `submit_verdict`'s `Blocked` branch builds the
  gate record and appends it to the chain (then discards the task and returns the
  remedy). `audit_len` counts blocked gates; `verify_chain` covers them.
- **`Workspace::merge_session(&self, message: &str) -> Result<(), WorkspaceError>`**
  added to the port (session-end effect). `InMemoryWorkspace`/`FsWorkspace` record it
  (inspectable no-op).
- **`GitWorkspace`** (new): real git effects against a target repo.
  - `GitWorkspace::create(repo: PathBuf, session: &str)` — creates session branch
    `kontur/<session>` in a dedicated `git worktree` under the repo's
    `.git/kontur-worktrees/<session>` (isolated from the user's checkout).
  - `apply_write` → file into the worktree; `run_command` → shell in the worktree.
  - `freeze_task_diff` → `git diff` bytes vs `HEAD` (canonical review artifact);
    `files`/`loc` from `git diff --numstat`. Empty diff → error (nothing to review).
  - `accept_task` → `git add -A && git commit` on the session branch (one commit per
    approved task).
  - `discard_task` → `git checkout -- . && git clean -fd` (worktree reset to `HEAD`).
  - `merge_session(message)` → squash-merge the session branch into the repo's
    original branch as **one commit** whose message carries the `Reviewed-by:`
    trailers (host composes the message from verified `reviewed_by` data), then
    remove the worktree. The audit chain, not git, remains the authoritative record.

### `kontur-net` (new crate — the attach/sync layer)

- **Protocol**: JSON-lines over any `AsyncRead + AsyncWrite` (TCP in production,
  `tokio::io::duplex` in tests). Messages:
  - client → server: `Hello { seat, operator }`, `Ready`, `Cast { gate_id, verdict:
    CastVerdict }`, `HandEdit { path, contents }`, `Rotate`, `Bye`.
  - server → client: `Welcome { seat }`, `State(WireState)`, `Rejected { reason }`.
- **`WireState`** — the sealing-safe shared snapshot broadcast to both seats:
  session phase (`AwaitOperators | DispatchReady { prompt } | PlanReview { tasks } |
  Executing | Closed`), per-seat presence/role/ready, fleet cards, capped log,
  the active gate (`WireGate { gate_id, task, files, loc, diff_hash, keys:
  Vec<VerdictView>, escalation_required, diff_preview }`), and at close a summary
  (gates, chain_verified, reviewers). Keys are `VerdictView` — sealed verdicts cross
  the wire as the data-free `Sealed` variant; there is no wire path for a sealed
  value.
- **`SessionServer`** — owns the `GateHost` + session state machine:
  - Two seats; `Hello` claims a seat by `OperatorId` (reconnect re-claims; presence
    tracked; disconnect ⇒ `linked=false` broadcast, gates **park**).
  - Dispatch gate: both seats `Ready` on the prompt → phase advances (PRD §6.1–6.2,
    simplified to a both-ready bar; live co-editing is deferred, noted in §4).
  - Plan review: the agent's proposed task list is broadcast; both seats `Ready` →
    approved (simplified both-ready bar; FR-7 edit-loop deferred).
  - Executing: gates from `GateHost.pending_gates()` stream into `WireState`;
    `Cast` routes to `submit_verdict` (rejection → `Rejected` to that seat only);
    `HandEdit` routes to `hand_edit`; `Rotate` swaps driver/navigator and logs.
  - Close: when the agent driver reports done and no gates pend — `merge_session`
    with `Reviewed-by:` trailers, then broadcast the audited summary.
- **`SessionClient`** — thin async client: connect, `Hello`, stream `State`s, send
  messages. Owns the seat's `Ed25519Signer` (seed-derived); signs `CastVerdict`s
  locally with a wall-clock `Clock` impl (the I/O layer may read time; the core
  stays pure). The host never sees a private key.
- **Agent driver**: a `ScriptedAgent` walks a small task list against the
  `GateHost` (writes → `begin_task_gate` → on `Blocked` applies the steer as a fix
  and re-proposes), reporting fleet status/log lines to the server. A **real agent**
  connects to the agent port, where each connection is served by the existing rmcp
  `KonturServer` over the TCP stream — same `GateHost`, same gates.

### `kontur-tui`

- **Remote mode**: build `SessionView` from `WireState` (new `ActiveRegion::Prompt`
  and `ActiveRegion::Plan` variants + renders); the app loop sends `Ready`/`Cast`/
  `HandEdit`/`Rotate` and re-renders on every `State`.
- **No-go wired end-to-end**: `[r]` opens remedy composition (input mode already
  exists); submit sends a signed `NoGo(Steer(text))` — the type system upstream
  still rejects bare vetoes.
- **Diff review wired**: `[o]` toggles a full-area diff pane rendering
  `diff_preview` (FR-24: the diff is actually openable). The deferred per-frame
  `diff_preview` computation is now consumed.
- **Hand-edit (minimal)**: `[e]` composes `path` then single-line contents; sends
  `HandEdit`. Crude but real; richer editing is later polish.
- **Honesty fixes**: `needs_you` counts gates where *this* seat's key is still
  `Awaiting` (not all pending gates); the close summary says `chain verified` and
  per-outcome counts rather than an unconditional "unanimous"; `linked=false`
  renders the §6.10 B-STATION DROPPED banner state.
- Local demo mode (`kontur demo`) retained.

### `kontur` binary (in `kontur-tui`)

- `kontur host --repo <path> [--operator-port N] [--agent-port N] [--prompt S]
  [--demo-agent] [--operators <hexA,hexB>]` — runs the `SessionServer` (TCP), the
  agent endpoint, and the scripted agent if requested.
- `kontur join --addr host:port --seat A|B --seed <n>` — attaches a console.
- `kontur demo` — the existing local single-station demo.
- Plain `std::env::args` parsing (no new CLI dependency).
- A real Claude Code attaches via a stdio↔TCP bridge (documented:
  `{"command": "nc", "args": ["<host>", "<agent-port>"]}`); *forcing* a harness's
  native tools through this endpoint remains the documented, deferred CC-binding
  concern.

### Docs (move with behaviour)

- `CLAUDE.md`: the "Stack & tooling" and "Build / run / test" sections are replaced
  with the real stack (Rust workspace; cargo build/test/clippy; the `kontur`
  subcommands).
- `README.md`: status updated from "Concept / pre-build" to a short "Running it"
  section.

## 3. Invariants — how this slice holds them

| # | Enforcement in this slice |
|---|---|
| 1 | Unchanged (`DualHold`); now exercised by two *distinct networked humans*, each signing client-side. |
| 2 | Unchanged (cast-time eligibility); the wire carries signed `CastVerdict`s that the engine still verifies against gate+diff. |
| 3 | Wire snapshots are built from `VerdictView` only; `Sealed` is data-free on the wire; `SealedVerdict` stays non-serializable. |
| 4 | `NoGo(Remedy)` unrepresentable without a remedy; the TUI's `[r]` composes the steer before a cast exists. |
| 5 | `HandEdit` applies immediately via the existing fresh-hold path; strict-mode exclusion + escalation signal surface on the wire. |
| 6 | **Closed**: `Outcome::Blocked` records chained + signed for blocked gates; every resolved gate is in the chain. |
| 7 | Disconnect ⇒ presence drop + park; no code path clears a hold with one key; reconnect resumes. |

## 4. Explicitly deferred (recorded, not hidden)

- Live collaborative prompt co-editing (CRDT) and plan *editing* — both-ready bars
  stand in for FR-4/FR-7's richer loops.
- Claiming (FR-3) — with one surfaced gate at a time it is not yet load-bearing.
- Risk tiers, scheduled rotation nudges, discuss threads, third-signatory
  escalation *action* (the signal is surfaced; the workflow is later).
- Forcing a harness's native tools through the MCP endpoint (the CC binding).
- Multi-gate queue UI; rich diff pager; richer hand-edit editing.

## 5. Testing strategy

- Unit/property tests per change (blocked-record chain verification incl. tamper;
  GitWorkspace against temp repos: freeze/accept/discard/merge with trailers;
  protocol serde round-trips incl. a sealed-key wire test asserting the JSON of a
  sealed key contains no verdict value).
- Server/client loopback tests over `tokio::io::duplex`: full session arc — two
  clients, ready → plan → gate → blind cast → sealed on the other seat → second
  cast → accept; a blocked variant with a steer; a disconnect/park/reconnect test.
- End-to-end test: in-process host on TCP + two `SessionClient`s + `ScriptedAgent`
  + `GitWorkspace` on a temp repo → session closes, `verify_chain` passes, the temp
  repo's original branch has exactly one squash commit whose message contains both
  `Reviewed-by:` trailers.
- TUI: golden tests for the new Prompt/Plan/diff-pane/dropped-link renders; the
  sealed-key golden test still asserts absence of a revealed value.
- Global: `cargo test` green, `cargo clippy --all-targets -- -D warnings` clean,
  no wall-clock/RNG in `kontur-core`, no `HashMap`/`HashSet` in canonical bytes.

## 6. Global constraints (carried into the plan)

- **No AI co-author trailers or generated-with footers on any commit** (global rule).
- Blind sealing never violated in any layer incl. the wire; never serialize
  `SealedVerdict`; never log a private key or sealed value.
- `accept_task`/`merge_session` reachable only via satisfied gates / session close.
- Terminal always restored; park-on-loss never degrades to one key.
- Stage only intended files; never `git add -A`; `/target` ignored.
- Edition 2021; clippy `-D warnings` clean; pristine test output.
