# UX Document — Kontur (КОНТУР-1)

> The experience of two engineers sitting at one console, supervising a fleet of coding agents. Derived from the Kontur PRD; read alongside it (this doc owns the *how it feels and behaves*, the PRD owns the *what and why*).

**Status:** Draft v0.1 · **Owner:** John · **Last updated:** 16 Jul 2026
*Mockups are reference states, not final layouts. Anything marked (TBD) is an open UX question, collected in §9.*

---

## 1. Purpose & scope

This document specifies the operator experience: the console's anatomy, its screen states through a full session, the keyboard-driven interaction model, and the two-operator co-op mechanics that make it a *pair* tool rather than a single-seat one. It does not restate requirements — where a behaviour is defined in the PRD, this doc shows how it *appears and is operated* and cites the PRD reference (e.g. FR-11, §10.1).

The north star: an operator who has used k9s or lazygit should feel at home in minutes, and should trust Kontur to sit in front of a production merge.

---

## 2. Design principles

1. **Every element is decision-relevant.** If an operator doesn't read it to decide or act, it isn't on screen. No host telemetry, no confidence scores, no ornament. (PRD §10.2)
2. **Emphasis is spent once.** The single thing that needs a human is loud; everything else is calm. A screen where three things shout is a screen where nothing does.
3. **Calm is the default.** The authentic control-room principle is *reducing* alarm noise, not manufacturing it. Kontur is quiet until it needs you.
4. **Keyboard-first, mouse-optional.** Every action has a key. The command line at the bottom is always live.
5. **The look is a consequence, not a costume.** Density and monospace fall out of supervising many things at once; the Cyrillic/version identity stays in the banner and never leaks into functional labels.
6. **Two operators, one truth.** Both see the same authoritative fleet state — except where independence *requires* divergence (blind sign-off, §5.4).

---

## 3. Who's in the seat

Two engineers, co-supervising. Roles are explicit and rotate (PRD FR-23):

- **Driver** — constructs the prompt / drives the active steer.
- **Navigator** — reviews live as the driver works, and by default leads the merge review (so the person signing off the output didn't author the instruction).

Rotation happens per task or per session; the console always labels who currently holds which role. Neither role is "senior" — the point is two independent vantage points, not a lead and an assistant.

> **Superseded (20 Jul 2026):** driver/navigator rotation is replaced by structural **Host/Operator** seats (the Host provides the agent backend; both seats are co-equal checkers; no rotation). Independence now rests on the two-distinct-keys requirement alone.

---

## 4. Console anatomy

> Note (21 Jul 2026): the `… tok` token counts in the mockups below are
> illustrative — live per-agent token telemetry is recorded future work and is
> not currently displayed (a placeholder zero was removed rather than shipped).


Persistent chrome, top to bottom. Regions are fixed; only their contents change by phase.

> **Superseded (20 Jul 2026):** single-screen two-pane layout (left: fleet + log activity; right: work surface). When a gate is pending, the diff is always visible in the right pane and verdicts are cast on the diff surface. The `[o]` diff-open toggle is removed.

```
========================================================================
[ КОНТУР-1  //  co-op session 4417  //  v0.4.2 ]      ← BANNER  (identity)
========================================================================
 LINK BOTH-STATIONS SYNC || 4-EYES ON || FLEET 3 (1 NEEDS YOU) || 6.4k tok
------------------------------------------------------------------------   ← STATUS STRIP
 [ STATIONS ]   who is here, their role, what they're doing            ← PRESENCE
 [ FLEET ]      one bordered panel per agent                           ← WATCH-FLOOR
 [ LOG ]        scrolling telemetry of real agent + operator actions   ← LOG
 [ GATE / PLAN / PROMPT ]   the active surface for the current phase   ← ACTIVE REGION
 >  command line — always live                                         ← COMMAND
```

- **Banner** — identity only. The one place the aesthetic flourish lives.
- **Status strip** — only operationally-real state: are both stations linked, is four-eyes armed, how many agents need you, session token spend (the real runaway signal). Nothing decorative.
- **Attention row** — one full-width line directly below the status strip, shown only when this seat has something to do or wait on. `loud` (BOLD + REVERSED) when THIS seat must act NOW; `dim` when waiting on the other seat. Absent when the fleet/log already convey the state (agent working, no gate). One row, never multiple lines — emphasis is spent once.
- **Stations** — presence and role for both operators (PRD FR-2).
- **Fleet** — the watch-floor. Agents that are just working stay calm; only an agent that needs a human is emphasised.
- **Log** — real actions you might need to intervene on, not flavour.
- **Active region** — swaps between PROMPT (dispatch), PLAN (task-list review), and GATE (sign-off) depending on phase.
- **Command line** — keyboard entry, always available.

---

## 5. Interaction model

### 5.1 Keyboard
Global keys are always live (dispatch, help, session). Panel actions apply to the focused panel. Gate actions (`[g]` go, `[r]` no-go+remedy, `[e]` hand-edit, `[d]` discuss) appear only when a gate is active. Additional keys: `[j]/[k]` scroll diff · `[tab]` select file · `[l]` invite link toggle · `[K]` abandon (confirm). Every on-screen action shows its key inline — no hidden verbs.

### 5.2 Claiming
The shared review queue and any active gate can be **claimed** by one operator, which shows on the other's console ("j.reed reviewing gate-03"). Claiming prevents both operators babysitting the same thing while other agents run unwatched (PRD FR-3). It's a soft signal, not a lock on the *other's* right to weigh in — but only one operator drives a given gate's interaction at a time.

### 5.3 Presence & rotation
Both stations always show. `[tab]` hands the driver role across; rotation is one keystroke and is logged. On rotation, the console re-labels roles everywhere so there's never ambiguity about who's driving.

Rotation can also be **scheduled**: at session start the operators agree an interval, and Kontur runs an **invisible timer** — no visible countdown, in keeping with the calm default — that quietly surfaces a rotation nudge when the interval elapses, at the next natural break (a gate boundary, never mid-review). The nudge is a dismissable suggestion, never a forced swap, and any rotation — manual or prompted — resets the timer. The point is to fight navigator fatigue without putting a clock on screen.

> **Superseded (20 Jul 2026):** driver/navigator rotation is replaced by structural **Host/Operator** seats (the Host provides the agent backend; both seats are co-equal checkers; no rotation). Independence now rests on the two-distinct-keys requirement alone.

### 5.4 The blind sign-off (where the two consoles diverge)
At a high-risk gate, the two operators **do not see the same thing** — by design. The first key is cast and *sealed*; the second operator reviews the change without seeing the first's verdict, so they can't anchor to it (PRD §10.1, FR-12). The consoles reconverge the instant both keys are in. This is the one deliberate break from "two operators, one truth," and it's the mechanism that makes the second signature worth more than the first.

### 5.5 Shared vs. private composition (21 Jul 2026)
Not every text field syncs the same way, and the difference is deliberate:

- **The dispatch prompt is a joint draft.** While either seat composes it,
  every keystroke streams to the other workstation (`PromptDraft`); both seats
  are writing one shared instruction, and consent is only ever signalled
  against text both can see.
- **A no-go steer is one seat's signed position.** It stays private while
  typed and becomes visible when cast, as part of the verdict. The other seat
  reacts to it after the fact — with its own steer or a hand-edit — rather
  than watching it form. Do not extend live draft-sync to steers; the privacy
  is the point.

---

## 6. Screen states

A full session, phase by phase.

### 6.0 Boot
On entry (host, join, or demo) the console shows a brief identity card —
the КОНТУР wordmark in block glyphs, the version, and one provenance line
(© Industrial Assets · open source · no warranty · repo URL) — centred in
the alternate screen for about a second before the session view takes over.
No animation, no input; it is a nameplate, not a splash.

### 6.1 Session start — idle
Both linked, no fleet, empty prompt buffer. The console invites an instruction.

```
========================================================================
[ КОНТУР-1  //  co-op session 4417  //  v0.4.2 ]
========================================================================
 LINK BOTH-STATIONS SYNC || 4-EYES ON || FLEET 0 || IDLE
------------------------------------------------------------------------
 [ STATIONS ]
 ┌─ A · YOU ─────────────────────┐ ┌─ B · J. REED ────────────────────┐
 │ HOST · idle                   │ │ OPERATOR · idle                  │
 └───────────────────────────────┘ └──────────────────────────────────┘
 [ PROMPT ]  no task dispatched — draft an instruction to begin
 ┌────────────────────────────────────────────────────────────────────┐
 │ _                                                                    │
 └────────────────────────────────────────────────────────────────────┘
 >  type to draft · [y] mark ready · [^↵] request dispatch
```

### 6.2 Prompt co-construction — the dispatch gate
Either seat drafts; the other reviews live and can suggest inline. This *is* the dispatch gate — a continuous review, not a separate checkpoint (PRD §5). The prompt can't be dispatched until both mark ready.

> **Supersession note (2026-07-20):** simplified in-console prompt entry is now implemented: `[p]` opens a compose line (empty seed); submitting replaces the prompt on both consoles and resets both ready flags — consent must re-signal against the text actually shown (same anchoring rule as the plan gate). The co-editing sketch below (dual-cursor inline suggestions) remains future work.

```
 [ PROMPT ]  drafting · host: you · operator reviewing live
 ┌────────────────────────────────────────────────────────────────────┐
 │ refactor the session guard in auth/session.ts to use the new        │
 │ token store; keep the public interface stable.                      │
 │ ~ nav: add a regression test for the expiry path        [a]ccept    │
 └────────────────────────────────────────────────────────────────────┘
 DISPATCH GATE   A you □ ready    B j.reed □ ready
 >  [^↵] dispatch — needs both ready
```

A reviewer's suggestion is a proposal the drafter accepts, which leaves a trace that the second seat actually engaged rather than passively watched.

### 6.3 Plan review
The agent analyses and returns a task list — a DAG of bounded, single-concern tasks (PRD FR-6). Both operators approve or edit before any code is written.

> **Implemented (2026-07-21, FR-7):** plan editing is live. `j`/`k` moves the selection cursor; `e` opens the current task text in the compose line for in-place editing; `d` deletes a task (blocked if only one remains); `<`/`>` reorders the selected task up or down. Any edit resets both ready flags — both seats must re-signal `y` against the current list before execution begins. The approved (possibly edited/reordered) list is returned to the agent via the `propose_plan` MCP response; the agent executes exactly that list in that order. Steer-first approach preferred: `[r]` sends a steer prompt to the agent to revise and re-propose; manual edits (`e`/`d`/`<`/`>`) remain available.

```
 ┌ PLAN ───────────────────────────────────────────────────────────┐
 │  ▶ t1  auth/session.ts   swap guard → token store              │
 │    t2  auth/tokens.ts    add expiry lookup                     │
 │    t3  auth/session.ts   thread expiry into guard              │
 │    t4  tests/session_*   regression: expiry path               │
 │  PLAN GATE   A ⟨□⟩ ready   B ⟨□⟩ ready                        │
 │  [r] steer replan · j/k select · e edit · d delete · </> move · [y] approve — needs both │
 └────────────────────────────────────────────────────────────────┘
```

### 6.4 Execution — the watch-floor
The default working view. Agents run through tasks sequentially; the fleet is calm except where a human is needed.

```
 LINK BOTH-STATIONS SYNC || 4-EYES ON || FLEET 3 (1 NEEDS YOU) || 6.4k tok
------------------------------------------------------------------------
 [ STATIONS ]
 ┌─ A · YOU ─────────────────────┐ ┌─ B · J. REED ────────────────────┐
 │ HOST · watching               │ │ OPERATOR · reviewing gate-03     │
 └───────────────────────────────┘ └──────────────────────────────────┘
 [ FLEET ]
 ┌─ AGENT-01 ────────────────────┐ ┌─ AGENT-02 ───┐ ┌─ AGENT-03 ──────┐
 │ analysing parser.py · 3.1k tok│ │ editing auth │ │ ▶ NEEDS SIGN-OFF│
 └───────────────────────────────┘ │ 1.2k tok     │ │ +47 -12 tests ok│
                                    └──────────────┘ └─────────────────┘
 [ LOG ]
 12:10:15 agent-01  patch → parser.py L45-52
 12:10:22 j.reed    claimed gate-03 · key sealed
 >
```

### 6.5 Merge gate — the dual-key sign-off

> **Superseded (20 Jul 2026):** the diff is permanently visible in the right pane while a gate pends; verdicts are cast on the diff surface. `[o]` is removed.

An agent parks its diff and enters the review queue (lifecycle `AWAITING_REVIEW`, PRD §8). The gate needs two keys. **Your view**, as the second, not-yet-cast key:

```
 ┌─ GATE-03 · agent-03 · auth/session.ts · +47 -12 · tests ok ────────┐
 │  KEY A  you / drv       □ awaiting your verdict                    │
 │  KEY B  j.reed / nav    ■ cast — sealed                           │
 │  [g] go   [r] no-go +remedy   [e] hand-edit   [d] discuss          │
 └────────────────────────────────────────────────────────────────────┘
 >  review the diff, then cast — j.reed's verdict stays sealed until you do
```

You cannot see j.reed's verdict. On *their* screen, the mirror: their own key shows cast, yours shows pending, and neither verdict is revealed. The instant both are in, both consoles reveal together:

```
 │  KEY A  you / drv       ■ GO                                       │
 │  KEY B  j.reed / nav    ■ GO     → unanimous · staging for merge   │
```

### 6.6 Intervention — no-go with a remedy
Pressing `[r]` will not accept a bare veto (PRD FR-13). The console demands the fix that goes back to the agent, and shows the downstream ripple.

```
 NO-GO · GATE-03 — a remedy is required (steer or edit)
 ┌────────────────────────────────────────────────────────────────────┐
 │ steer > token lookup re-hits the store every request. cache it      │
 │         per-session, cap TTL at 5m, evict on logout.                │
 └────────────────────────────────────────────────────────────────────┘
 >  [↵] send steer to agent-03 · [e] switch to hand-edit
 [ LOG ]
 12:14:02 you     no-go gate-03 + steer · agent-03 → rework
 12:14:02 kontur  downstream t4 flagged for re-review (depends t3)
```

### 6.7 Hand-edit — the emergency override
A hand-edit applies to the worktree *immediately* — the catastrophe-aversion path (PRD FR-15/FR-16). But it doesn't merge until the combined diff clears a **fresh** sign-off, and authorship is flagged.

```
 HAND-EDIT · agent-03 · auth/session.ts   [applied to worktree now]
 ┌────────────────────────────────────────────────────────────────────┐
 │ your edit is live. authorship: hand-edited.                         │
 │ the combined diff must clear a fresh sign-off before it can merge.  │
 └────────────────────────────────────────────────────────────────────┘
 GATE-03 (re-opened · combined diff)
   KEY A  you / drv      ■ GO      (pragmatic mode: editor may co-sign)
   KEY B  j.reed / nav   □ awaiting — co-signer must be a non-editor ✓
 >
```

*Strict-mode variant:* the editor is a maker and cannot sign. With only two operators, the gate then can't reach two eligible keys and **escalates to a third signatory** per the availability policy (§9, PRD §10.1). The console makes that state explicit rather than silently letting the editor self-approve.

### 6.8 Discuss — gate-anchored notes (21 Jul 2026)
`[d]` composes a short note attached to the active gate; both seats see the
thread as a **DISCUSS** strip on the gate surface (it appears only when the
gate has notes and there's room), so operators can align in-band without
switching to out-of-band chat. Notes are communication only — they do **not**
pause verdicts and do not by themselves change the recorded outcome.

> Future work: the fuller design — a side-thread that *pauses* verdicts and
> records that a gate needed discussion (outcome `resolved-after-disagreement`,
> PRD FR-14) — is not yet built. Today, `resolved-after-disagreement` is
> recorded when a gate actually resolves after a no-go+remedy, independent of
> the discuss thread.

```
 DISCUSS · GATE-03   (verdicts paused)
 ┌────────────────────────────────────────────────────────────────────┐
 │ j.reed: the cache TTL isn't bounded — leak risk under churn?         │
 │ you:    fair — cap at 5m, evict on logout. steer it.                 │
 └────────────────────────────────────────────────────────────────────┘
 >  [r] no-go +that steer   [g] go anyway   [esc] resume verdicts
```

### 6.9 Session close — merge & audit
All tasks approved; the staged set merges as one reviewed commit carrying the maker-checker trailers (PRD FR-21).

```
 SESSION 4417 · COMPLETE   4/4 tasks approved · merged to main
 ┌────────────────────────────────────────────────────────────────────┐
 │ commit  a1b9f2c  "refactor session guard → token store"            │
 │ Reviewed-by: you        Reviewed-by: j.reed                        │
 │ audit   4 gates · 1 hand-edit · 1 resolved-after-disagreement       │
 │ chain   verified ✓ (tamper-evident)                                 │
 └────────────────────────────────────────────────────────────────────┘
 >  [v] view audit record · [n] new session
```

### 6.10 Exception states
Kept quiet until they need a decision.

**Agent failed / stuck:**
```
 ┌─ AGENT-02 ── FAILED ──────────┐  cannot resolve import
 │ ▶ needs decision              │  [r]etry  [p]rompt  [k]ill
 └───────────────────────────────┘
```

**Operator disconnect — the four-eyes-can't-clear edge:**
```
 LINK B-STATION DROPPED · 4-EYES CANNOT CLEAR
 ┌────────────────────────────────────────────────────────────────────┐
 │ j.reed disconnected 00:42 ago. gates needing a second key are held. │
 │ availability policy: PARK (default) · [w]ait   [e]scalate to 3rd    │
 └────────────────────────────────────────────────────────────────────┘
```
Because two distinct keys are structurally required, losing an operator *stalls sign-offs by design* — the console surfaces the availability policy rather than degrading to single-key approval.

---

## 7. Key journeys

- **Clean task.** Dispatch → plan approved → agent works t1 → both go → repeat → merge. Calm throughout; the console never raised its voice.
- **Caught in review.** Agent-03's diff parks; the Operator claims it, spots an uncached lookup, casts no-go with a steer; agent reworks; t4 re-flagged; second pass goes unanimous. The audit shows a `resolved-after-disagreement` — evidence the second pair of eyes did work.
- **Emergency.** Agent about to write a destructive migration; either seat hand-edits to guard it, applied instantly; combined diff re-signed by both (pragmatic mode) before merge; audit flags the task `hand-edited`.
- **Partner steps away.** The Operator drops mid-session; open gates park; the Host either waits or escalates to a third signatory — never self-approves.

---

## 8. Micro-copy & tone

Terse, operational, lower-case for machine chatter, plain English for anything functional. Verbs are actions (`dispatch`, `cast`, `steer`, `hand-edit`), not marketing. No exclamation-driven urgency; the layout carries emphasis, not the words. The identity register (КОНТУР-1, session numbers, version stamp) is confined to the banner. If a label reads like set dressing, it gets cut.

---

## 9. Open UX questions

1. **Diff review surface.** Where and how the full diff renders for review — inline expand under the gate, a dedicated pane, or handoff to the operator's pager/editor. (The gate shows the summary; the deep read is TBD.)
2. **Availability policy default & escalation UX.** Park-indefinitely vs escalate-to-third; how a third signatory is nominated and joins mid-session.
3. **Blind vs live sign-off by tier.** Which risk tiers seal the first verdict and which reveal live — and how the operator knows which mode a gate is in.
4. **Dispatch-gate depth.** The prompt-review interaction is sketched (§6.2) but the "ready" bar and how the reviewing seat's suggestions are tracked need their own pass (mirrors PRD open question #3).
5. **Fleet scale.** Layout behaviour when the fleet is larger than the panels comfortably fit — scroll, collapse-calm-agents, or a density mode.
6. **Onboarding.** First-run experience for operators who haven't used a TUI of this density.
