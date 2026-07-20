# Tier 1 — real-agent completion: design + plan

**Date:** 2026-07-20 · **Owner:** John · **Status:** executing (autonomous directive)

Closes the four Tier-1 gaps: real Claude Code end-to-end (incl. permission-level
native-tool blocking and an agent-proposed plan phase), FR-24 diff-opened approval with
truthful review depth, the failure lifecycle (kill-switch / FAILED), and wire
encryption with certificate pinning.

## 1. Real Claude Code end-to-end

**Plan phase from the agent.** New MCP tool `propose_plan { tasks: [string] }` on
`KonturServer`: parks the call until both seats approve the plan (mirror of
`propose_task_complete`'s await). Machinery in `GateHost`:
- `propose_plan(tasks: Vec<String>) -> watch::Receiver<bool>` stores the proposed plan,
  emits `HostEvent::PlanProposed { tasks }`, returns the approval watch.
- `approve_plan()` resolves it (operator face; called by the session server when both
  seats are ready in PlanReview).
- `proposed_plan() -> Option<Vec<String>>` accessor.

**Session flow (net).** `PlanReview` renders the agent's *actual* tasks when a
`PlanProposed` event arrives (before that: "waiting for agent plan…" and both-ready is
refused for an empty plan). Both-ready in PlanReview calls `GateHost::approve_plan()`
(releases the parked MCP call) *and* the legacy `plan_tx` (scripted agent unchanged).

**Spawning CC (`kontur host --claude`).** After the dispatch gate clears (observed on
the state watch), the host spawns:
`claude -p "<protocol prompt incl. session prompt>" --mcp-config <generated>.json
 --allowedTools "mcp__kontur__*" --disallowedTools "Write" "Edit" "MultiEdit" "NotebookEdit" "Bash"`
- The generated MCP config bridges stdio→TCP via `nc 127.0.0.1 <agent-port>`.
- **Enforcement is permission-level, not instruction-level**: native file/shell tools
  are denied via CC's own flag system. (Honest caveat retained: this relies on CC's
  permission system, not an OS sandbox.)
- Child exit → `agent_done` (status 0) or a `FAILED` fleet card (non-zero) with the
  gates left parked.
- Command construction is a pure, unit-tested function; actual spawning stays thin.

**Testing.** The e2e "real agent" is an in-process rmcp client over TCP (the same
client shape as `server_mcp.rs`): connects to the agent endpoint, calls `propose_plan`
(blocks) → seats approve → writes + `propose_task_complete` → verdicts → close. Proves
the entire new machinery without needing the `claude` binary in tests.

## 2. FR-24 — approval requires the opened diff; truthful depth

- The console tracks, per gate id, whether the diff pane was opened. `g` on an unopened
  diff is refused with a status hint ("open the diff first — [o]"); after opening, the
  cast is signed with `ReviewDepth::FullDiff`. A `no-go` remains castable without the
  diff (refusal is always allowed) and is then signed `ReviewDepth::Summary` —
  the signed record now attests what actually happened.
- `SessionClient::cast_go/cast_nogo` take a `ReviewDepth` parameter.

## 3. Failure lifecycle (FR-10/22 subset)

- `ClientMsg::Abandon` — either seat's kill-switch (`[k]`, with a y/N confirm in the
  console). Server: discard all pending tasks, no merge, phase
  `Closed { abandoned: true, merged: false, .. }`; console renders `SESSION ABANDONED —
  nothing merged` loudly. Audit chain keeps whatever gates already resolved.
- Agent child exiting non-zero (or exiting with gates pending) → fleet card `FAILED`;
  gates park as always; operators steer (`r`), hand-edit (`e`), or abandon (`[k]`).

## 4. Wire encryption with pinning

- `tokio-rustls` + `rcgen`: the host generates a per-session self-signed cert; the
  operator listener speaks TLS. The invite link gains the SHA-256 of the
  DER-encoded certificate as the fingerprint: `kontur://ip:port/<token>#<fp-hex>`;
  the joining client verifies the pinned fingerprint (no CA, no hostname check —
  the pin is the trust root, and the link already travels a private channel).
  Old un-pinned links are rejected by the new client with a clear error.
- The agent endpoint stays localhost-plaintext (CC connects via local `nc`); the agent
  endpoint is hard-bound to 127.0.0.1 (remote agents are not a supported topology yet).

## Constraints carried

All existing invariants and gates unchanged; no co-author trailers; clippy `-D
warnings` clean; conflation-safe tests (StateCursor/watch semantics); no
`HashMap`/`HashSet` in canonical bytes; docs move with behaviour (README/CLAUDE.md/PRD
notes in the final task).

## Task plan (SDD)

1. **mcp: plan machinery** — GateHost plan state + `PlanProposed` event + `propose_plan`
   MCP tool (blocking) + `approve_plan`/`proposed_plan`; tests incl. a blocked-until-
   approved rmcp round-trip in `server_mcp.rs`.
2. **net: plan flow + real-agent e2e** — event-pump handles `PlanProposed`; PlanReview
   shows real tasks / refuses empty-plan ready; both-ready → `approve_plan` + legacy
   `plan_tx`; new e2e `real_agent_over_tcp` (rmcp client) covering plan→gate→close.
3. **bin: `--claude`** — pure `build_claude_command(...)` + config generation + spawn
   on dispatch-clear + child-exit handling (`agent_done`/FAILED); unit tests for the
   command/config; README/CLAUDE.md attach docs replaced by `--claude`.
4. **FR-24** — depth param on client casts; per-gate opened-diff tracking + refusal in
   the console; tests (client depth propagation; refusal logic pure-fn tested).
5. **failure lifecycle** — `Abandon` protocol + server handling + `[k]`+confirm in the
   console + abandoned close render; FAILED card on child exit (bin); tests.
6. **TLS pinning** — rcgen cert, tokio-rustls listener/client, `#fp` link fragment,
   pin verification; loopback tests incl. wrong-pin rejection.
7. **docs + final whole-branch review** — sweep docs, run the final review, fix, merge.
