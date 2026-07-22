# CLAUDE.md

Guidance for Claude Code (and any agent) working in this repository. Read it before making changes.

---

## Project: Kontur (КОНТУР-1)

**A brutalist CLI for high-efficiency agentic pair programming.**

Kontur is a text-based, terminal-native workstation where **two engineers jointly supervise a fleet of coding agents** on the same codebase. One hosts the agents, one joins to co-sign; both review as co-equal checkers. The product's entire reason to exist is a single guarantee:

> **Nothing an agent writes reaches `main` without two independent human approvals.**

Everything in this file exists to keep that guarantee — and the decisions that support it — intact as the code grows.

---

## Source of truth

Design decisions live in the doc set. Treat them as authoritative:

- **`README.md`** — what Kontur is, in brief.
- **The PRD** (`docs/PRD-coop-supervisor.md`) — problem, requirements (FR-*), architecture, and the two-signatory four-eyes mechanism (§10.1).
- **The UX doc** (`docs/UX-kontur.md`) — console anatomy, screen states, interaction model.

**If a request contradicts a decision recorded in these docs, stop and flag it — do not silently diverge in code.** These decisions were argued through carefully; surface the conflict rather than relitigating it in an implementation. When a change alters behaviour the PRD or UX describes, update the doc in the same change.

---

## Non-negotiable invariants

The product's value *is* these properties. Never weaken, shortcut, or "simplify" them. If a task appears to require breaking one, that is a signal to stop and raise it — not to proceed.

1. **Two independent keys to merge.** No code path may let a single operator's approval merge a change. Two *distinct* operator identities, both `go`.
2. **Independence enforced at acceptance, not display.** A verdict cast by the change's maker (prompt author / hand-editor) is not counted in strict mode. Check eligibility when a verdict is cast, never only when it's shown.
3. **Blind second review.** On high-risk gates the first verdict is *sealed* — it must not be observable in state, logs, API responses, or UI until the second verdict is cast.
4. **No bare veto.** A `no-go` without a remedy (a steer prompt or a hand-edit) is rejected.
5. **Hand-edit: instant apply, deferred acceptance.** A hand-edit takes effect in the worktree immediately, but never becomes merge-eligible without a fresh dual sign-off. It is recorded as human-authored and never folded into the agent's diff.
6. **Tamper-evident audit.** Every gate emits a signed record, hash-chained to the previous one. Never break the chain, never emit an unsigned decision, never make a past gate outcome mutable.
7. **Fail safe under operator loss.** If an operator drops, gates needing a second key **park**. Never degrade to single-key approval to keep things moving.

---

## Architecture (current direction)

- **Enforcement plane: MCP.** Agents work through the hosted MCP server: `write_file`/`run_command` execute in the isolated worktree and are recorded (streamed live to the console); the **gated boundary is `propose_task_complete`**, which parks the task's frozen diff at a dual-hold until both keys resolve it. Claude Code connects to the agent endpoint through `kontur mcp-bridge <port>` (a built-in stdio↔TCP bridge, no external `nc`). The two-signatory logic lives between MCP's pause and resume — MCP provides the primitive; the four-eyes hold is ours. The agent also proposes its plan through a gated `propose_plan` call that blocks until both seats approve. Claude Code's native mutation tools are denied at spawn via its own permission flags (`--allowedTools`/`--disallowedTools`); an OS-level sandbox remains future work.
- **Agent backend: Claude Code** (sole backend for MVP). Keep backend-specific glue behind a thin adapter so multi-backend stays possible later — but do not build multi-backend now. Enforcement relies on Claude Code's own permission flags, not an OS-level sandbox.
- **Topology:** one shared host holds the repo and runs the fleet; the Host's terminal is itself a seat; the Operator attaches over the network. Each agent gets a **git worktree**; approved work accumulates on a session branch and **merges once at the end** as a single reviewed commit with `Reviewed-by:` trailers.
- **Client:** a **text-based TUI**. Two seats, one shared authoritative state, with presence, soft gate claiming (`[c]`), and gate-anchored discuss notes (`[d]`). The Host invites the operator with one 52-char code (`kontur://ip:port/<code>` = base32 of a 128-bit invite secret, from which the operator's key is derived, plus a 128-bit pinned TLS cert fingerprint — magic-link model, or `--byo` where the operator brings a persistent key the host approves by fingerprint). The prompt is composed in-console at the dispatch gate ([p]); every edit resets both ready marks.

---

## Design principles — apply to ALL UI / TUI code

**Brutalist: raw, structural, honest.** Concretely:

- **Every on-screen element must be decision-relevant.** No decorative telemetry (host CPU), no false-precision scores (agent "confidence %"), no alarms that don't mean anything.
- **Emphasis is spent once** — only the thing that needs a human is loud; everything else stays calm.
- **Keyboard-first.** Every action has a key, shown inline. No hidden verbs.
- **Identity flourish stays in the header** (КОНТУР / version banner) — never in functional labels.
- **Benchmark against k9s, lazygit, btop** — tools an SRE leaves open all day. If it looks like a hacker-movie prop, it's wrong.
- **Copy is terse and operational.** Verbs are actions (`dispatch`, `cast`, `steer`, `hand-edit`).

---

## Glossary — use these terms consistently

- **Seat / station** — one of the two human consoles.
- **Host / Operator** — the two seats, both co-equal operators. Displayed as **"Operator A [Host]"** (the seat that hosts and provides the agent backend) and **"Operator B"** (the seat that joins remotely). Both are co-equal checkers: either can review, sign, steer, or hand-edit. Roles are structural (who hosts), not rotating.
- **Gate** — a point requiring sign-off. **Dispatch gate** (is the prompt ready?) and **merge gate** (is the change good?).
- **Dual-hold** — the state object that holds a parked action until two keys resolve it. It is the internals of the `AWAITING_REVIEW` lifecycle state.
- **Key** — an operator's signed verdict (`go` / `no-go`). **Sealed** — a cast-but-hidden verdict (blind review).
- **Steer** — a corrective prompt to an agent. **Hand-edit** — a direct human code change.
- **Fleet** — the set of agents. **Task** — a bounded change scoped to the *smallest single concern* (not strictly one file).

---

## Stack & tooling

**Rust workspace** (`edition = "2021"`), four crates:

| Crate | Role |
|---|---|
| `kontur-core` | Four-eyes engine: hold, verdict, audit chain, signing |
| `kontur-mcp` | MCP enforcement plane (`rmcp`), gate host, git workspace |
| `kontur-net` | Session server/client, protocol codec, scripted agent, MCP agent endpoint |
| `kontur-tui` | Ratatui TUI, `kontur` binary |

Key runtime deps: `rmcp 2.2`, `ratatui 0.30`, `tokio 1`, `ed25519-dalek` (via kontur-core).

## Build / run / test

```sh
# build everything
cargo build

# build the kontur binary specifically
cargo build -p kontur-tui --bin kontur

# run tests (whole workspace)
cargo test

# lint
cargo clippy --all-targets -- -D warnings

# run the self-contained local demo
cargo run -p kontur-tui --bin kontur -- demo

# zero-config host in cwd (must be a git repo with at least one commit)
cd your-project && kontur

# join as operator — paste the invite link the host printed:
kontur join kontur://…

# host a session (in-memory workspace; demo scripted agent)
cargo run -p kontur-tui --bin kontur -- host --mem --demo-agent

# host with a real Claude Code agent (primary path); prompt composed in-console:
cd your-project && kontur --claude
# kontur spawns claude with --allowedTools mcp__kontur__* and --disallowedTools
# Write Edit MultiEdit NotebookEdit Bash once both seats approve the dispatch gate.
# Agent output goes to a session log (path shown persistently as a host-only footer in the console, and printed on startup).

# join as operator (legacy --addr/--seed form; still works)
cargo run -p kontur-tui --bin kontur -- join --addr 127.0.0.1:7777 --seed 2
```

---

## Working agreements

- **Dogfood the philosophy:** keep changes small and single-concern.
- **Flag, don't relitigate:** if a request conflicts with a recorded decision, raise it rather than coding around it.
- **Docs move with behaviour:** a change that alters PRD/UX-described behaviour updates that doc in the same commit.
- **Security is load-bearing:** this tool handles operator **signing keys** and a **tamper-evident audit chain**. Never log secrets, never weaken signature generation or verification, never make audit records mutable. Treat all crypto and gate-logic as high-risk code that warrants extra care and review.

---

## Status & future work

**Working today (v0.2):** the four-eyes engine (`kontur-core`), the MCP enforcement plane with real git effects (`kontur-mcp`), the two-seat networked session with live agent-activity streaming (`kontur-net`), and the console + `kontur` binary (`kontur-tui`). Zero-config hosting (`kontur` in a git repo); **real Claude Code as the agent** (`--claude`, permission-restricted at spawn); agent clarification questions (on genuine prompt ambiguity the agent calls `ask_clarification`; both operators answer multiple-choice, with a custom option; divergent answers reconcile via [A / B / accept both]; resolved answers return to the agent before it plans); agent-proposed plan gate; in-console prompt entry with live draft sync (each keystroke streams to both seats; edits reset consent), multi-line + bracketed-paste-safe with cursor editing (alt+↵ newline; paste inserts verbatim, never submits); a slow-flashing caret marks the text-entry point in every compose mode (real terminal cursor, blinked at a controlled ~600ms cadence); compact 52-char invites (derived keys + pinned-TLS fingerprints) with a LAN/WAN toggle; blind dual sign-off; FR-24 (a `go` requires the opened diff; review depth signed truthfully); no-go-with-steer; hand-edit; park-on-disconnect (with application keepalive — client Pings every 15s, server times out at 45s so half-open/NAT-dropped links park honestly; the operator console raises a loud HOST LOST banner when its own link to the host goes silent); AFK presence (`[z]` — either seat marks itself away; shown on both consoles, attention calmly says "waiting on X (AFK)"; presence only, so gates needing the away seat's key still park — never single-key merge; cleared on reconnect); session kill-switch (`[K]` abandon, agent stopped, nothing merges); blocked-and-satisfied gates both audited; **audit chain persisted at close** (`.kontur/audit-<head>.json`, content-addressed, written on merge and abandon; `kontur audit <file>` verifies offline; merge commit carries an `Audit-chain:` head trailer); session squash-merge with `Reviewed-by:` trailers. **Boot screen** (КОНТУР wordmark, version, provenance; ~1 s, then the console). **Structural two-pane layout:** left pane (fleet + log), right pane (diff + verdict bar); diff never collapses and always displays while gate is pending; **per-file diff sections** — the diff is split per file on the server, each section independently capped at 64 KiB (one huge generated file cannot starve the others), `[tab]` cycles files, scroll resets per file; diff scrollable with `[j]`/`[k]` (vim pairing), log scrollable with `[↑]`/`[↓]` (title shows the scrollback offset, tail-sticks when at bottom); files bar drops on small screens; LOC count in files bar; **dispatched instruction stays visible** — a TASK line in the left pane carries the locked prompt through plan review and execution; **command outcomes surfaced** — the activity log marks non-zero exits loudly and the gate card shows the task's last command + exit status. FR-24 satisfied by structural requirement that diff is always visible and verdicts cast on diff surface. **In-viewer `$EDITOR` hand-edits** (`[e]` while gate pending): fetches file from server, suspends TUI, launches `$EDITOR`/`vi`, sends `hand_edit` if changed. **FR-7 plan editing:** in-TUI j/k/e/d/</>/y during PlanReview; any edit resets both ready flags; approved/edited list returned to agent via `propose_plan` response.

**Recorded future work** (see PRD §9/§13 for detail): audit-chain hardening (operator-side record replication + external chain-head anchoring via Rekor/OpenTimestamps/RFC 3161 — a deliberate non-blockchain decision, reasoning in PRD §9); operator-supplied keys with host-side approval (**implemented** — `kontur --byo` puts seat B on a zero sentinel; the operator joins with `kontur join --byo` using a persistent `~/.kontur/operator_key`, presents its own key, and the host approves the fingerprint in-console (`[a]`/`[x]`); the approved key is bound into seat B and the gate roster, a cast is bound to the seat's authenticated identity, and `kontur id` shows a fingerprint to read out-of-band); OS-level sandboxing of the agent (enforcement today is CC's own permission flags); real per-agent token/cost telemetry (placeholder display removed — the agent's propose_task_complete token reports still feed the audit provenance, but there is no trustworthy live per-agent count yet); risk tiers; multi-agent fleets (in progress — SOLO BY DEFAULT; an agent proposes a split into independent parallel streams and operators approve it, work stays small-single-concern and clearly attributed per agent/task. **Decision half landed:** per-agent identity through the enforcement plane (#77), the `propose_split` gate (#79), and the operator Split-approval phase — both `[y]` approve / one `[n]` decline (#80). **Execution half remaining:** per-agent worktree isolation, host fan-out (spawn a sub-agent per approved stream), aggregate merge).

---

## Non-goals

- **No embedded video, no in-app comms** — operators use their existing chat (e.g. Slack) out-of-band.
- **No rich GUI / IDE** — text TUI only.
- **No centralised SaaS** running agents on someone else's machine — code stays where the team already trusts it. (No relay/hole-punching either; cross-network pairing goes over a mesh VPN — Tailscale/WireGuard — using the LAN-style invite.)

## Platform scope

Developed/tested on macOS and Linux. Windows is untested (path handling and the `$EDITOR` hand-edit unvalidated) — treat a Windows host as unsupported until a compatibility pass lands. No external-tool dependencies (`nc` replaced by the built-in `kontur mcp-bridge`).
