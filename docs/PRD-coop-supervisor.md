# PRD — Kontur (КОНТУР-1)

> A terminal-native tool that lets **two engineers jointly supervise a shared fleet of coding agents** on the same codebase, with human review gated at each step and a tamper-evident audit trail.

**Status:** Draft v0.1 · **Owner:** John · **Last updated:** 16 Jul 2026
*Sections marked (proposed) are drafted for reaction, not yet decided. Sections marked (TBD) are known gaps.*

---

## 1. Summary

Automated agentic engineering optimises for **speed with a single operator** — one human dispatches a prompt and waits for a result. That is the right trade-off for solo developers, but it quietly breaks a control that regulated and enterprise engineering organisations depend on: **segregation of duties** (four-eyes / maker-checker). When an agent is the maker and the same lone engineer is the only checker — or no one is — there is no independent second review of what ships.

Co-op Supervisor reintroduces the pair, one layer up. Instead of two people at one keyboard writing code, two people jointly supervise a fleet of agents writing the code: co-constructing the instruction, reviewing the plan, and independently signing off on every change before it merges. The by-product of the workflow is a complete, signed audit trail that a risk or compliance team can trust.

---

## 2. Problem

- **Pairing broke in the agentic era.** Pair programming assumed a driver typing code and a navigator watching the cursor. Once the driver stops typing code, there is nothing for the navigator to navigate, so pairing feels awkward and tends to be abandoned.
- **Solo agentic tooling doesn't fit assurance-first contexts.** Existing multi-agent supervisors are built for one operator running many agents fast. Enterprises that cannot adopt fire-and-forget precisely *because* of dual-control requirements are unserved.
- **Agentic development silently violates four-eyes.** The agent is the maker; in solo automated flows the instructing engineer is also the sole checker. For a regulated production deployment that is a control gap audit and risk teams will flag.

---

## 3. Goals & Non-goals

### Goals
1. Make prompt construction a **deliberate, co-reviewed act** rather than one person typing into a void.
2. Enforce **independent dual human review** of every change an agent produces, before it can merge.
3. Give two operators a **live shared overview** of a fleet of agents and **task-level control** to steer, reject, or hand-edit individual work.
4. Emit a **tamper-evident audit artifact** as a natural by-product of the workflow — not reconstructed from logs afterward.
5. Keep it **terminal-native** and close to where engineers already work.

### Non-goals
- Replacing solo, fire-and-forget agentic tooling (different buyer, different job).
- A centralised SaaS that runs agents on someone else's machine (code stays where the team already trusts it).
- Replacing CI, code hosting, or the merge platform — this sits in front of them and hands off.
- Full IDE / rich GUI, embedded video, or an in-app comms channel — Kontur stays a text-based TUI; operators use their existing call/chat (e.g. Slack) out-of-band.

---

## 4. Users & buyer

- **Primary users:** two engineers acting as co-supervisors (driver + navigator, rotating).
- **Context:** enterprise / regulated production environments where two-engineer peer review is required or strongly preferred, even for agentically-generated code.
- **Buyer (proposed):** engineering leadership answerable to risk, audit, or compliance — the people who currently *cannot* sign off on autonomous agentic deployment because segregation of duties isn't provable.

---

## 5. Core concepts

- **Maker-checker / four-eyes.** The agent is the maker. Two humans are independent checkers. The prompt author is recorded as the *instructing party*; a policy toggle controls whether the author may also be a checker (strict mode: no; pragmatic mode: yes, but a second checker who did not instruct is always required). (policy — TBD default)
- **Driver / navigator, rotating.** The driver constructs the prompt; the navigator reviews it live as it's written. Both review worker output. Roles rotate per task or session so the second set of eyes stays genuinely open and so the person reviewing output didn't co-author the spec. Rotation is the primary lever for keeping the two reviews *independent* — two correlated reviews (both rubber-stamping) are one review counted twice.

  > **Superseded (20 Jul 2026):** driver/navigator rotation is replaced by structural **Host/Operator** seats (the Host provides the agent backend; both seats are co-equal checkers; no rotation). Independence now rests on the two-distinct-keys requirement alone.
- **Two gates, not one.**
  - **Dispatch gate** — before an agent runs: is the prompt/task well-specified? (maker-checker on the *instruction*).
  - **Merge gate** — before a change lands: is the output correct and complete? (maker-checker on the *result*).
- **Shared fleet, thin clients.** Both operators supervise **one** fleet against **one** codebase, rather than each running a private fleet and reconciling over git.

---

## 6. End-to-end workflow

1. **Co-construct the prompt.** In a multiplayer session, either supervisor can edit the prompt; the other reviews live (default: driver types / navigator suggests; optional simultaneous two-cursor editing for low-conflict touches such as fixing a typo while the other types). This *is* the dispatch gate — the navigator's continuous review, not a separate checkpoint.
2. **Dispatch → plan.** The prompt is sent to the agent. The agent analyses the work and returns a **task list**: each task a single-purpose change scoped to the **smallest single concern** (one logical change, however many files it honestly touches — not strictly one file).
3. **Approve the plan.** Both supervisors approve or edit the task list. Editing loops back to re-review.
4. **Execute, one task at a time.** On approval, the agent works through tasks sequentially. Each completed change is presented to both operators.
5. **Review each change.** For every task the change is either:
   - **Approved by both** → staged on the session worktree;
   - **Rejected with a steer prompt** → returned to the agent to rework; **or**
   - **Hand-edited** by either supervisor.
   A no-go — including a **split** where only one supervisor disagrees — must carry its remedy (a steer or an edit); **no bare vetoes**. Supervisors discuss and supply the fix.
6. **Ripple.** Any non-approval (rejection, hand-edit, or a supervisor editing a task) makes the agent **reconsider downstream tasks**; any resulting changes to later tasks require fresh approval.
7. **Merge.** When every task is approved, the staged set merges to main as one reviewed commit carrying the audit trailers.

---

## 7. Functional requirements

### Multiplayer session & presence
- **FR-1** Two operators attach to one shared session (authoritative fleet + state live in one place; operators are thin clients).
- **FR-2** Live presence: each operator can see what the other is viewing/claiming (e.g. "Alice reviewing task 4").
- **FR-3** A shared review/"needs-you" queue either operator can pull from, with **claiming** to prevent redundant double-supervision. *(gate claiming implemented 2026-07-21: `[c]` toggles a soft presence claim on the active gate — "reviewing" shown to the other seat, released on toggle/gate-resolution/disconnect; presence only, never affects verdict eligibility. A shared multi-item queue awaits multi-agent fleets.)*

### Prompt co-construction (dispatch gate)
- **FR-4** Collaborative prompt editing; default driver-types / navigator-suggests, with optional simultaneous two-cursor editing (opt-in, per session). *(simplified in-console prompt entry with edit-resets-consent implemented 2026-07-20; live draft sync implemented 2026-07-21 — each keystroke streams to both seats via `PromptDraft`, resets both ready flags, and is unlogged until the commit; Esc restores the pre-edit text; simultaneous drafts are last-write-wins. **Two-cursor co-editing deferred indefinitely (21 Jul 2026): the live draft sync is sufficient; revisit only if simultaneous editing becomes a real pain point.**)*
- **FR-5** A prompt cannot be dispatched without the dispatch gate being satisfied (both operators have seen/accepted the instruction). (exact bar — TBD)

### Planning
- **FR-6** The agent returns a task list of bounded, single-concern tasks with explicit dependencies (a DAG).
- **FR-7** Both operators can approve or edit the task list; edits re-enter plan review. *(implemented 2026-07-21: `EditPlan` wire message; in-TUI j/k select, e edit, d delete, </> reorder; any edit resets both ready flags; approved/edited list returned to agent via `propose_plan` response)*; prompt-based replan steers supported and preferred — a steer withdraws the proposal and routes the remedy to the agent, which re-proposes

### Execution & lifecycle
- **FR-8** Tasks execute sequentially against a per-agent isolated worktree.
- **FR-9** On completion, a task parks at the merge gate with its diff frozen and enters the shared review queue.
- **FR-10** The system implements the full task lifecycle in §8, including exception states (blocked / failed / abandoned).

### Review & dual approval (merge gate)
- **FR-11** A task is **APPROVED** only on **both** operators' go (unanimity required).
- **FR-12** For high-risk gates, present the change to the second reviewer **before** revealing the first's verdict ("blind second review") to reduce anchoring. (proposed; ties to risk-tiering §7 last item)
- **FR-13** A no-go must be accompanied by a steer prompt or a hand-edit describing the fix. Bare rejection is not a valid terminal action.
- **FR-14** Split decisions (one go, one no-go) route to the intervention path and are **recorded as splits**; outcome is at least `{unanimous, resolved-after-disagreement}`.

### Hand-edit / emergency override
- **FR-15** Hand-editing is **always available** and **never gated** — an experienced supervisor must be able to step in to avert a catastrophe. (Reserve for extreme cases by convention, but never remove.)
- **FR-16** A hand-edit takes effect in the working tree **immediately** (instant effect), is fed back to the agent so it's aware, but **cannot enter the merge set until both operators sign off** the combined diff (deferred acceptance).
- **FR-17** Authorship is recorded per task as a flag set (`agent` / `hand-edited` / `both`) so mixed provenance is never misrepresented. The approval bar is identical regardless of authorship.

### Intervention & downstream replan
- **FR-18** Rejection, hand-edit, or a supervisor task-edit triggers agent re-planning of downstream tasks; changed downstream tasks require fresh approval before execution continues.
- **FR-19** Bounded up-front planning is the primary defence against earlier approved tasks being invalidated. There is **no agent-driven backward invalidation**; an already-approved task reopens **only** when supervisors deliberately edit it mid-session, which then re-ripples forward through re-approval.

### Audit & provenance
- **FR-20** Every gate emits a signed, immutable audit record at gate time (see §9), hash-chained to the previous record so the sequence is tamper-evident.
- **FR-21** The final merge commit carries maker-checker **git trailers** (`Reviewed-by: …` per operator) plus a content-addressed link to the full record. An inline session transcript in the commit message is **optional**; capturing the record itself is not.

### Exception handling
- **FR-22** Blocked (dependency unresolved), Failed (agent error/stuck → human decision: retry / re-prompt / abandon), and Abandoned (supervisor kill-switch, terminal) states are first-class. *(kill-switch/ABANDONED implemented 20 Jul 2026; FAILED surfaced on agent exit)*

### Independence & rotation
- **FR-23** Role rotation (driver ↔ navigator) supported per task or session; by default the operator who navigated the prompt leads the merge review.

  > **Superseded (20 Jul 2026):** driver/navigator rotation is replaced by structural **Host/Operator** seats (the Host provides the agent backend; both seats are co-equal checkers; no rotation). Independence now rests on the two-distinct-keys requirement alone.
- **FR-24** Approvals require genuine engagement — an operator cannot approve from a summary alone; the actual diff must be opened. (proposed) *(implemented: go requires the opened diff; review depth recorded truthfully in the signed verdict — 20 Jul 2026)* — superseded by the split layout (20 Jul 2026): the active gate's diff is permanently on-screen; acceptance happens on the diff surface by construction; truncated diffs still require explicit acknowledgment.

### Risk-tiering (proposed / optional)
- **FR-25** Optional per-path risk tiers: low-risk tasks may allow lighter review; flagged paths (auth, payments, migrations) always require both operators, full-diff review, and green tests. *Note: current decision is uniform dual approval everywhere; tiering is offered as a throughput valve to consider, not a committed requirement.*

---

## 8. Task lifecycle state machine

See the accompanying diagram (`task-lifecycle.svg`). States:

| State | Meaning |
|---|---|
| PLAN_PROPOSED | Agent has returned the task list; awaiting plan approval |
| PLAN_APPROVED | Task list signed off; execution begins |
| PLAN_REVISION | Downstream tasks re-planned after an intervention; awaiting re-approval |
| PENDING | Task queued (bounded, single-concern) |
| IN_PROGRESS | Agent working the task |
| AWAITING_REVIEW | Change complete, diff parked at merge gate, in shared queue |
| INTERVENED | Rejected-with-prompt or hand-edited (hub node) |
| APPROVED | Both operators approved; staged on session worktree |
| MERGED | (session-level) all approved tasks merged to main with audit trailers — terminal |
| BLOCKED | Dependency unresolved |
| FAILED | Agent error/stuck; needs human decision |
| ABANDONED | Killed by supervisors — terminal |

Key transitions (happy path): `PENDING → IN_PROGRESS → AWAITING_REVIEW → APPROVED`, then a session-level roll-up to `MERGED`. Intervention: `AWAITING_REVIEW → INTERVENED`, fanning out to `IN_PROGRESS` (rework), `PLAN_REVISION` (reconsider downstream), and back to `AWAITING_REVIEW` (hand-edit re-review). All three previously-undefined transitions are now resolved (see §5 policy toggle, FR-14, FR-16, FR-19).

**Design note:** INTERVENED is the busiest node — reject, hand-edit, and downstream-replan all converge there, and it is the only state that mutates the plan mid-flight. It is where concurrency bugs and audit ambiguity will concentrate, and should be over-specified first.

---

## 9. Audit record (first cut)

Each gate record captures:

- **Provenance:** task id + DAG position; verbatim prompt/spec + author; agent id, model, and version; diff content-hash, files, LOC; the agent's tool trail (reads/writes/commands); token & cost.
- **Checks:** for each checker — identity, timestamp, verdict (`go` / `no-go` / `conditional-go`), and **review depth** (full diff / summary / tests run); conditions/comments; independence assertion (checker ≠ prompt author, or a flagged exception).
- **Authorship flag:** `agent` / `hand-edited` / `both`.
- **Outcome:** `unanimous` / `resolved-after-disagreement`.
- **Integrity:** hash-chained to the prior gate record; each verdict signed with a per-operator key (non-repudiation).

Precedent: the Linux kernel already does maker-checker in git via `Signed-off-by:` / `Reviewed-by:` trailers — this formalises and enforces the same pattern.

**Residual integrity gap & planned hardening (future functionality, 20 Jul 2026).** The chain is tamper-*evident* (any mutation fails verification) but a party controlling the only stored copy could truncate or discard it — evidence that vanishes isn't evidence. Two planned mitigations close this without a blockchain (a full distributed ledger solves multi-writer consensus, a problem a one-host/two-signer session does not have):

1. **Operator-side record replication** — both consoles retain an independent copy of every gate record as it is emitted; the host cannot truncate what the counterparty holds.
2. **External anchoring** — at session close (optionally per gate), publish the 32-byte chain-head hash to a witness outside the host's control: Sigstore Rekor (public append-only transparency log), OpenTimestamps, or an RFC 3161 TSA for maximum auditor familiarity. Only hashes leave the machine.

---

## 10. Technical architecture (high-level, TBD)

- **Shared host model.** One machine holds the repo and runs the fleet; both operators attach over the network as TUI clients. This sidesteps replicating/merging a live filesystem across two remote humans — there is one authoritative state, two viewers. (Remote-pair topology — TBD.)
- **Isolation.** Git worktree per agent so parallel edits don't collide; approved tasks accumulate on a session branch; single reviewed merge at the end.
- **Build-on.** Prospective foundation is Claude Code's hook system (PreToolUse and Task lifecycle hooks give the event stream and the approval interception) plus Agent Teams (fleet + shared task list). Prior art proves the single-operator hook-to-TUI approval loop (e.g. `iris`); the **novel layer to build** is multi-client attach + presence + claim + dual-approval arbitration, and the two-seat TUI.
- **MCP as the enforcement / audit chokepoint.** Route the agents' consequential actions (file writes, shell, merge) through MCP servers the tool hosts, and use MCP's invocation-level approval (`require_approval` / queue-then-execute) as the gate. This makes the gate and its audit record **backend-agnostic**, and lets MCP's `blocking` vs `audit` gate types express the dual-approval vs instant-hand-edit split directly. Two extensions on top: a standard MCP approval gate is *single-approver*, so the two-signatory (four-eyes) requirement is layered over it; and native harness tools bypass MCP unless actions are forced through the hosted servers. Lifecycle / stream / steer remains a per-harness adapter concern, not something MCP abstracts.
- **Prompt co-edit.** CRDT (e.g. Yjs/Automerge-class) for the shared prompt buffer.

### 10.1 Two-signatory approval — the four-eyes extension over MCP

The one mechanism no existing framework provides, and the reason the product exists: turning MCP's single-approver gate into an independent, dual, signed sign-off. It sits **between MCP's pause and MCP's resume** — MCP gives the pause/resume primitive (queue-then-execute); the two-signatory logic is our orchestration over it.

**The dual-hold.** When a consequential action reaches its gate, the hosting MCP server parks the invocation and hands control to the supervisor host, which opens a *dual-hold*: a state object carrying the pending invocation, its diff hash, the accumulating verdict set, and the gate policy (required signatories, independence mode, risk tier). The host resumes-and-executes the MCP call **only** when the hold reaches SATISFIED. States:

- **OPEN** — 0 verdicts.
- **PARTIAL** — 1 verdict cast (on high-risk tiers, sealed and hidden from the second reviewer).
- **SATISFIED** — 2 `go` verdicts from 2 distinct eligible operators → resume + execute.
- **BLOCKED** — ≥1 `no-go` → action discarded, route to INTERVENED with the attached remedy.

The dual-hold *is* the internal machinery of the AWAITING_REVIEW lifecycle state (§8): one hold per gated action.

**Distinct, eligible signatories.** Each verdict is signed by a distinct operator identity — two different keys; a second verdict from the same key is rejected. Eligibility follows the independence policy (§5): *strict* — neither signatory may be the prompt author or hand-editor of this change (a maker cannot check their own work); *pragmatic* — the author/editor may be one of the two, but the co-signer must be a non-author. Eligibility is enforced at verdict-acceptance time, not at display — an ineligible operator can view and comment but cannot cast a counting verdict.

**Blind second review (anchoring control).** On high-risk tiers, verdicts are *sealed on commit and revealed only once both are in*: A reviews, casts, seals; B reviews the change without seeing A's verdict, casts, seals; both reveal. This stops the second review collapsing into a rubber-stamp of the first — the property that makes two signatures worth more than one. Low-risk tiers may reveal live. Blinding is per-tier policy, not global.

**No bare veto.** A `no-go` is rejected unless it carries a remedy payload — a steer prompt or a reference to a hand-edit (FR-13). On a valid `no-go` the hold goes BLOCKED and the task routes to INTERVENED with the remedy, driving the rework/replan ripple (FR-18).

**Hand-edit through the hold (instant apply, deferred sign-off).** A hand-edit is not a gated MCP call; it is a direct human write applied to the worktree **immediately** — modelled as MCP's non-blocking `audit` type (executes now, record created), never held (FR-15). But it becomes merge-eligible only after its *combined* diff clears a **fresh** dual-hold: the edit lands, authorship is flagged `hand-edited`/`both`, and the task re-enters AWAITING_REVIEW for two sign-offs. Under strict independence the hand-editor is now a maker on this change and cannot be one of the two signatories.

**Signing & record.** Each verdict is signed with the operator's identity key. The signature pair, invocation/diff hash, review-depth, timestamps, authorship flag, and outcome (`unanimous` / `resolved-after-disagreement`) compose the gate record (§9), hash-chained to the prior one. Signatures give non-repudiation — a sign-off can't be disowned or forged. The final merge's `Reviewed-by:` trailers (FR-21) are derived from these signatures.

**Concurrency & availability.** Verdict acceptance on a hold is atomic (single-writer / optimistic locking), so two operators acting at once can't double-count and one operator can't cast both verdicts; claiming (FR-3) reduces contention upstream. Because two *distinct* sign-offs are structurally required, a hold cannot clear with only one operator present. The **availability policy is park, always** — a stalled gate waits for its second key indefinitely and never degrades to one. There is deliberately **no third signatory**: if the two operators cannot agree, that is theirs to resolve (discuss, then one casts a no-go with a remedy), not something the system routes around. *(decided 21 Jul 2026; the earlier "escalate to a third after a timeout" option is dropped.)*

**How it layers on MCP (summary).** MCP contributes the pause/resume primitive, the non-blocking `audit` record for hand-edits, and elicitation for rendering each operator's review form. Everything four-eyes-specific — the N=2 hold, distinct-key eligibility, blind sealing, no-bare-veto, signing and chaining — lives in the supervisor host. That is the part you build, and the part nobody else has.

### 10.2 UX / design language

The interface is a **terminal-native, ASCII-windowed operational console** — the aesthetic of a control room, not a product dashboard. The reference point is a Soviet-era nuclear-power-station workstation: dense panelled read-outs, monospaced status fields, blocky bordered windows, an unfussy utilitarian palette.

This is more than styling — it reinforces the thesis. A nuclear control room is the canonical two-person-rule, high-assurance, continuously-monitored environment, so the metaphor carries the product's identity: each agent reads as a monitored channel, the fleet as a panel of gauges, the go/no-go poll as a console action weighty enough to deserve ceremony. The look signals *assurance-first* the moment an operator sees it — and it aligns with the mission-control / dual-key analogies the whole design grew out of.

Practical implications: each agent gets a bordered ASCII panel (status, current task, token/cost, pending-approval indicator); the shared review queue and the dual-hold render as console alerts an operator claims and acts on; the two-operator go/no-go presents as a paired console poll with the second key sealed until cast. (Detailed layouts — TBD.)

**Practical, not cheesy.** The discipline that keeps this from tipping into costume: every element on screen must be something an operator reads to decide or acts on — no decorative telemetry (host CPU, fake link sweeps), no false-precision confidence scores, no blanket "CRITICAL" banners. Emphasis is spent once, on the single thing that needs a human; a calm default *is* the authentic control-room principle (post-Three-Mile-Island HMI design is about reducing alarm noise, not manufacturing it). Benchmark against tools an SRE leaves open all day — k9s, lazygit, btop — not a hacker-movie prop. The Cyrillic / version-banner identity flourish stays confined to the header.

---

## 11. Differentiation

The market splits into two camps, and neither is this:

- **One human, many agents** — terminal fleet supervisors (Batty, iris, construct, Galley). Great single-operator ergonomics; no second human.
- **Many humans, many *separate* fleets** — team layers (e.g. AgentsRoom) where each developer runs a local fleet and coordinates asynchronously through git.
- **Multiplayer but cloud/GUI** — proposals like GitHub Next's "Agent Collaboration Environment" (shared cloud microVMs, chat-driven).

**Whitespace:** two humans, **one shared fleet**, same codebase, terminal-native, with **four-eyes structurally enforced** and an audit artifact as output. The enforced dual-control-for-regulated-environments angle is the wedge — it targets exactly the buyers who can't adopt fire-and-forget.

---

## 12. Success metrics (proposed)

- Adoption by teams with dual-control requirements (the unserved segment).
- Defects/regressions caught at the merge gate that a single reviewer would have missed (measures the value of the *second* set of eyes).
- Rate of `resolved-after-disagreement` outcomes (evidence reviews are independent, not rubber-stamped).
- Audit completeness: % of merges with a full signed, chained record.
- Time-to-merge *with assurance* vs. the team's prior manual dual-review baseline (are we faster than their status quo, not faster than fire-and-forget).

---

## 13. Phasing (proposed)

- **MVP:** shared host + two-seat TUI; Claude Code as sole backend; **MCP as the action/enforcement plane** (consequential actions routed through hosted MCP servers, gated for approval); dispatch gate as live co-edit; sequential execution; merge gate with unanimous dual approval; hand-edit; audit record + git trailers.
- **v1:** downstream replan/ripple; presence/claim polish; blind second review; risk-tiering; rotation support.
- **Later:** multi-agent parallelism within a session; multiple agent backends; richer observability; **audit-chain hardening** — operator-side record replication + external chain-head anchoring (Rekor / OpenTimestamps / RFC 3161), see §9; operator-supplied keys with host-side approval (replacing magic-link invites).

---

## 14. Open questions

1. **Naming — chosen: Kontur** (styled КОНТУР-1 / KONTUR-1). Operational / control-room register, matching §10.2. (Alternates considered: Keypair, Soyuz, Containment.)
2. **Independence policy default** — strict (author may not check) vs pragmatic (author may be one of two)?
3. **Dispatch gate** — the prompt-side flow is currently one arrow; needs its own mini-spec (what "well-specified" means, and its approval bar).
4. **Attach protocol** — the multi-client state-sync + claim/arbitration model. The load-bearing engineering piece.
5. **Remote-pair topology** — shared host assumed; confirm for the "working remotely on the same codebase" case.
6. **Disagreement capture depth** — do we log the discussion, or only that a split occurred and how it resolved?
7. **Risk-tiering** — commit to uniform dual approval, or introduce tiers as a throughput valve?
8. **Agent backend & enforcement plane — decided.** MVP commits to **MCP as the action/enforcement plane** (consequential actions — writes, shell, merge — routed through hosted MCP servers and gated for approval) and **Claude Code as the sole backend**. Multi-backend and the per-harness adapter generalisation are deferred (§13, Later). The MCP fit and its caveats are captured in §10; what remains here is a build task, not an open question — the two-signatory extension over MCP's single-approver gate.
