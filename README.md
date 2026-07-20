# KONTUR-1 · КОНТУР-1

**A brutalist CLI for high-efficiency agentic pair programming.**

Two engineers, one console, a fleet of agents — and nothing ships without both keys.

<img width="1135" height="836" alt="Screenshot 2026-07-20 at 11 50 28" src="https://github.com/user-attachments/assets/b62decff-a735-4616-b056-1dfa1d08eabd" />

---

## What it is

KONTUR (kohn-toor) is a terminal-native workstation where two engineers jointly supervise a fleet of coding agents on the same codebase. One hosts the agents, one joins to co-sign; both review. Every change an agent makes is gated behind two independent human approvals before it can merge, and the whole session leaves a signed, tamper-evident audit trail.

It exists for the place solo agentic tooling can't go: production environments where *the agent wrote it and one person glanced at it* isn't good enough, and segregation of duties is the rule, not a nicety.

## The idea

Pair programming assumed a driver at the keyboard and a navigator watching the cursor. Agentic development took the keyboard away — so pairing moves up a layer. The **Host** runs the session and provides the agents (the Claude Code connection lives on their machine); the **Operator** joins over the network. Both construct and review instructions, both watch the fleet, and both independently approve every change. Maker-checker, with the agent as maker and two co-equal humans as checkers.

## How it works

- **Compose the prompt.** Either seat drafts/edits it in-console ([p]); every edit resets both ready marks; both mark ready to dispatch.
- **Plan first.** The agent returns a task list of bounded, single-concern changes. Both seats approve the plan before a line is written.
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
 │  KEY A  HOST · you      □ awaiting your verdict                    │
 │  KEY B  OPERATOR j.reed ■ cast — sealed                           │
 │  [g] go   [r] no-go +remedy   [e] hand-edit   [d] discuss          │
 └────────────────────────────────────────────────────────────────────┘
```

## Principles

Brutalist: raw, structural, honest. Every element on screen earns its place or it's cut — no decorative telemetry, no confidence theatre, no alarms that don't mean anything. The look is a consequence of supervising many things at once, not a costume applied over it. Calm until it needs you.

## Running it

```sh
# Install (from a clone of this repo):
cargo install --path crates/kontur-tui

# Zero-config: host in your current git repo — your terminal becomes the HOST console,
# with the invite shown in-console until the operator links ([l] toggles LAN/WAN link):
cd your-project && kontur

# Operator: paste the invite link the host sends you:
kontur join kontur://…

# Self-contained local demo (both seats, scripted agent, in-memory workspace):
cargo run -p kontur-tui --bin kontur -- demo

# Host a real session on a git repo with a scripted demo agent:
cargo run -p kontur-tui --bin kontur -- host --repo /path/to/repo --demo-agent

# Host with a custom prompt and operator seeds (backward-compat, integer form):
cargo run -p kontur-tui --bin kontur -- host --mem --prompt "add auth gate" --seeds 1,2

# Join as operator (legacy --addr/--seed form still works):
cargo run -p kontur-tui --bin kontur -- join --addr host:7777 --seed 2
```

Invite links carry the operator's key — send privately; operator-supplied keys with host-side approval are future work.

The `host` command also binds an MCP endpoint (default port 7778). Use `--claude`
to have kontur spawn a permission-restricted Claude Code agent automatically once
both seats approve the dispatch gate:

```sh
# Primary path — kontur spawns the agent for you:
kontur host --prompt "add auth module" --claude

# The agent launches with native file/shell tools denied at CC's permission level;
# only mcp__kontur__* tools are allowed. Output goes to a session log (path printed
# on startup).
```

Alternatively, attach Claude Code manually (advanced / alternative):

```sh
# 1. Save as kontur-mcp.json:
{"mcpServers":{"kontur":{"command":"nc","args":["127.0.0.1","7778"]}}}

# 2. Run:
claude --mcp-config kontur-mcp.json \
  --allowedTools "mcp__kontur__*" \
  --disallowedTools Write Edit MultiEdit NotebookEdit Bash \
  --permission-mode default \
  -p "<your protocol prompt>"
```

Agent writes, commands, and gate openings stream live into the operator console
as they happen — no keypress needed. Every task completion parks at a four-eyes
gate until both operators cast a verdict. Enforcement is permission-level: native
mutation tools are denied via CC's `--allowedTools`/`--disallowedTools` flags.
Enforcement relies on Claude Code's own permission flags, not an OS-level sandbox.
The operator wire is TLS-encrypted with the cert pinned via the fingerprint embedded
in the invite link.

The design lives in:

- **`PRD-coop-supervisor.md`** — problem, requirements, architecture, and the two-signatory four-eyes mechanism.
- **`UX-kontur.md`** — console anatomy, screen states, and the interaction model.

## Built on

MCP as the action/enforcement plane (consequential actions routed through hosted MCP servers and gated for approval), with Claude Code as the first agent backend. The two-signatory sign-off is layered over MCP's single-approver gate — the part nobody else has built.
