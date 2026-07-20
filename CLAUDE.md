# CLAUDE.md

Guidance for Claude Code (and any agent) working in this repository. Read it before making changes.

---

## Project: Kontur (КОНТУР-1)

**A brutalist CLI for high-efficiency agentic pair programming.**

Kontur is a text-based, terminal-native workstation where **two engineers jointly supervise a fleet of coding agents** on the same codebase. One drives, one navigates; both sign off. The product's entire reason to exist is a single guarantee:

> **Nothing an agent writes reaches `main` without two independent human approvals.**

Everything in this file exists to keep that guarantee — and the decisions that support it — intact as the code grows.

---

## Source of truth

Design decisions live in the doc set. Treat them as authoritative:

- **`README.md`** — what Kontur is, in brief.
- **The PRD** (`PRD-coop-supervisor.md`) — problem, requirements (FR-*), architecture, and the two-signatory four-eyes mechanism (§10.1).
- **The UX doc** (`UX-kontur.md`) — console anatomy, screen states, interaction model.

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

- **Enforcement plane: MCP.** Consequential actions (file writes, shell, merge) route through **hosted MCP servers** and are gated via MCP's invocation-level approval (`require_approval` / queue-then-execute). The two-signatory logic lives **between MCP's pause and resume** — MCP provides the primitive; the four-eyes hold is ours.
- **Agent backend: Claude Code** (sole backend for MVP). Keep backend-specific glue behind a thin adapter so multi-backend stays possible later — but do not build multi-backend now.
- **Topology:** one shared host holds the repo and runs the fleet; the Host's terminal is itself a seat; the Operator attaches over the network. Each agent gets a **git worktree**; approved work accumulates on a session branch and **merges once at the end** as a single reviewed commit with `Reviewed-by:` trailers.
- **Client:** a **text-based TUI**. Two seats, one shared authoritative state, with presence and claiming.

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

- **Operator / station** — a human seat (A and B).
- **Host / Operator** — the two seats. The Host's terminal runs the session and provides the agent backend (the Claude Code connection); the Operator joins remotely. Both are co-equal checkers: either can review, sign, steer, or hand-edit. Roles are structural (who hosts), not rotating.
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

# host a session (in-memory workspace; demo scripted agent)
cargo run -p kontur-tui --bin kontur -- host --mem --demo-agent

# join as operator (after `host` prints the join line)
cargo run -p kontur-tui --bin kontur -- join --addr 127.0.0.1:7777 --seed 2
```

---

## Working agreements

- **Dogfood the philosophy:** keep changes small and single-concern.
- **Flag, don't relitigate:** if a request conflicts with a recorded decision, raise it rather than coding around it.
- **Docs move with behaviour:** a change that alters PRD/UX-described behaviour updates that doc in the same commit.
- **Security is load-bearing:** this tool handles operator **signing keys** and a **tamper-evident audit chain**. Never log secrets, never weaken signature generation or verification, never make audit records mutable. Treat all crypto and gate-logic as high-risk code that warrants extra care and review.

---

## Non-goals

- **No embedded video, no in-app comms** — operators use their existing chat (e.g. Slack) out-of-band.
- **No rich GUI / IDE** — text TUI only.
- **No centralised SaaS** running agents on someone else's machine — code stays where the team already trusts it.
