# KONTUR-1 · КОНТУР-1

**A brutalist CLI for high-efficiency agentic pair programming.**

Two engineers, one console, a fleet of agents — and nothing ships without both keys.

---

## What it is

Kontur is a terminal-native workstation where two engineers jointly supervise a fleet of coding agents on the same codebase. One drives, one navigates; both sign off. Every change an agent makes is gated behind two independent human approvals before it can merge, and the whole session leaves a signed, tamper-evident audit trail.

It exists for the place solo agentic tooling can't go: production environments where *the agent wrote it and one person glanced at it* isn't good enough, and segregation of duties is the rule, not a nicety.

## The idea

Pair programming assumed a driver at the keyboard and a navigator watching the cursor. Agentic development took the keyboard away — so pairing moves up a layer. The driver constructs the instruction; the navigator reviews it live; the agents write the code; both engineers independently approve every change. Maker-checker, with the agent as maker and two humans as checkers.

## How it works

- **Co-write the prompt.** The navigator reviews as the driver types; both mark ready before it dispatches.
- **Plan first.** The agent returns a task list of bounded, single-concern changes. Both operators approve the plan before a line is written.
- **One task at a time.** Agents work sequentially; each finished change parks at a gate.
- **Two keys, independent.** On high-risk gates the first verdict is sealed until the second is cast — no anchoring, no rubber-stamp. A no-go must carry its fix. A hand-edit applies instantly for emergencies but still needs both keys before it merges.
- **Merge with a trail.** The approved set lands as one reviewed commit carrying `Reviewed-by` trailers and a link to a hash-chained audit record.

## The console

```
 LINK BOTH-STATIONS SYNC || 4-EYES ON || FLEET 3 (1 NEEDS YOU) || 6.4k tok
 ┌─ AGENT-01 ────────────────────┐ ┌─ AGENT-02 ───┐ ┌─ AGENT-03 ──────┐
 │ analysing parser.py · 3.1k tok│ │ editing auth │ │ ▶ NEEDS SIGN-OFF│
 └───────────────────────────────┘ └──────────────┘ └─────────────────┘
 ┌─ GATE-03 · agent-03 · auth/session.ts · +47 -12 · tests ok ────────┐
 │  KEY A  you / drv       □ awaiting your verdict                    │
 │  KEY B  j.reed / nav    ■ cast — sealed                           │
 │  [g] go   [r] no-go +remedy   [e] hand-edit   [d] discuss          │
 └────────────────────────────────────────────────────────────────────┘
```

## Principles

Brutalist: raw, structural, honest. Every element on screen earns its place or it's cut — no decorative telemetry, no confidence theatre, no alarms that don't mean anything. The look is a consequence of supervising many things at once, not a costume applied over it. Calm until it needs you.

## Running it

```sh
# Self-contained local demo (both seats, scripted agent, in-memory workspace):
cargo run -p kontur-tui --bin kontur -- demo

# Host a real session on a git repo with a scripted demo agent:
cargo run -p kontur-tui --bin kontur -- host --repo /path/to/repo --demo-agent

# Host with a custom prompt and operator seeds:
cargo run -p kontur-tui --bin kontur -- host --mem --prompt "add auth gate" --seeds 1,2

# Join as operator A (run this on each operator's machine):
cargo run -p kontur-tui --bin kontur -- join --addr host:7777 --seat A --seed 1
cargo run -p kontur-tui --bin kontur -- join --addr host:7777 --seat B --seed 2
```

The `host` command also binds an MCP endpoint (default port 7778). A real agent
(Claude Code or compatible) connects via a stdio bridge such as:

```json
{"command": "nc", "args": ["localhost", "7778"]}
```

The agent's `write_file` and `propose_task_complete` calls are gated — every
task completion parks until both operators cast a verdict. Binding native Claude
Code tool calls through this endpoint is the next integration step.

The design lives in:

- **`PRD-coop-supervisor.md`** — problem, requirements, architecture, and the two-signatory four-eyes mechanism.
- **`UX-kontur.md`** — console anatomy, screen states, and the interaction model.

## Built on

MCP as the action/enforcement plane (consequential actions routed through hosted MCP servers and gated for approval), with Claude Code as the first agent backend. The two-signatory sign-off is layered over MCP's single-approver gate — the part nobody else has built.
