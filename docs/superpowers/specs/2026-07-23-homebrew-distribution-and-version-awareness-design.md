# Design: Homebrew distribution + version awareness

**Date:** 2026-07-23
**Status:** Approved — ready for implementation planning
**Topic:** Make Kontur easy to install and keep current via Homebrew, with an in-app
upgrade signal and peer-version awareness between the two seats.

---

## Problem

Kontur has no release artifacts today: no tags, no GitHub Releases, no packaging, no
release automation. Installing means cloning the repo and running `cargo build`. There is
also no signal to an operator that a newer version exists, and no way for the two seats to
notice they are running mismatched releases.

We want:

1. `brew install` / `brew upgrade kontur` as the primary install + upgrade path.
2. A calm in-app notice when a newer version is available.
3. A calm nudge when the two seats are on different releases, encouraging them to align.

All of this must respect Kontur's brutalist UX rules (emphasis spent once; every on-screen
element decision-relevant) and its security posture (no telemetry, no code leaves the host,
audit/crypto paths untouched).

---

## Scope

Four cohesive pieces, all about *version*:

- **A. Release pipeline** — GitHub Actions builds prebuilt binaries on each `v*` tag.
- **B. Homebrew tap** — a custom tap whose formula installs those prebuilt binaries.
- **C. In-app upgrade check** — an async, fail-silent check against the GitHub Releases API.
- **D. Peer-version awareness** — the two seats exchange release versions at handshake.

### Decisions locked during brainstorming

1. **Prebuilt binaries + custom tap** (not build-from-source, not homebrew-core). Fast
   installs, no Rust toolchain required of users. GitHub Releases are the substrate.
2. **In-app GitHub Releases check** for the upgrade signal (not shelling out to `brew`,
   which would only work for brew installs and adds a runtime dependency).
3. **Homebrew only for MVP.** GitHub Releases make a future manual download / `curl|sh`
   *possible*, but we build only the brew path now. No `cargo install` docs, no curl
   installer. (YAGNI.)
4. **Documented install leads with the tap-first form** (see §B) — `brew install
   user/repo` is not a valid formula reference (Homebrew reads two-part `user/repo` as a
   *tap*, per the Homebrew docs), so the short command requires a one-time `brew tap`.
5. **Notices live in the footer, on both seats** (host *and* operator) — not the boot
   screen, not the activity log.
6. **`PROTOCOL_VERSION` bumps 8 → 9** because Hello/Welcome gain version fields (§D).
7. **Crate versions unified** via `[workspace.package]` so one bump covers the workspace
   and the tag ↔ version check is unambiguous (§E).

---

## A. Release pipeline — `.github/workflows/release.yml`

- **Trigger:** push of a tag matching `v*`.
- **Pre-flight guard:** assert the tag (`v0.2.0`) matches the workspace crate version
  (`0.2.0`). A mismatch fails the workflow before anything is built or published.
- **Build matrix (targets):**
  - `aarch64-apple-darwin` (macOS Apple Silicon) — `macos-14` runner.
  - `x86_64-apple-darwin` (macOS Intel).
  - `x86_64-unknown-linux-gnu` (Linux x86_64) — `ubuntu-latest`.
  - `aarch64-unknown-linux-gnu` (Linux arm64) — **deferred**, added when there is demand.
- **Per-job build:** `cargo build --release -p kontur-tui --bin kontur`, package the
  binary as `kontur-<version>-<target>.tar.gz`, and emit a matching `.sha256`.
- **Publish:** attach every tarball + checksum to a single GitHub Release for the tag.
- **Formula bump:** a final job checks out the tap repo, rewrites the per-platform `url`
  and `sha256` in `Formula/kontur.rb` to the new release, and commits. Auth via a
  narrowly-scoped PAT stored as an Actions secret (write access to the tap repo only).

**Notes**
- Linux binaries use the default `gnu` target for MVP. If glibc portability bites, revisit
  `x86_64-unknown-linux-musl` (noted risk: `ring`/rustls musl builds can be finicky).
- macOS binaries are **not** Apple-notarized. Homebrew strips the `com.apple.quarantine`
  xattr when it installs from a downloaded tarball, so unsigned binaries run cleanly via
  brew. Notarization is future work (see §Out of scope).

---

## B. Homebrew tap — `industrial-assets/homebrew-kontur`

A new repository, `industrial-assets/homebrew-kontur` (tap name: `industrial-assets/kontur`),
containing `Formula/kontur.rb`.

Because the binaries are prebuilt, the formula only selects the correct tarball per OS/arch
and installs it — **no `depends_on "rust"`**, installs in seconds:

- `on_macos` + `on_arm` / `on_intel` → the matching `aarch64`/`x86_64` darwin tarball.
- `on_linux` → the `x86_64-unknown-linux-gnu` tarball.
- `def install; bin.install "kontur"; end`
- `test do` → runs `kontur --version` and asserts the version string.

**Documented install (README leads with the tap-first form):**

```sh
brew tap industrial-assets/kontur
brew install kontur          # bare name works once tapped
```

Upgrades: `brew upgrade kontur`. A no-fuss one-liner alternative (auto-taps) is mentioned
as secondary: `brew install industrial-assets/kontur/kontur`.

---

## C. In-app upgrade check

A small, self-contained module (in `kontur-tui`) that runs on **interactive** startup
(`host` / `join`; skipped for `demo`, scripted agents, and tests).

**Behaviour**
- Spawns an **async, non-blocking** task — startup never waits on it.
- Reads a cache file `~/.kontur/update-check.json` holding `{ last_checked, latest_version }`.
- If the cache is older than **24h** (or absent), performs **one** unauthenticated GET to
  the GitHub Releases "latest" API for `industrial-assets/kontur`, with a **≤3s timeout**,
  and rewrites the cache.
- Compares the latest tag against `CARGO_PKG_VERSION` using semver.
- If a newer version exists, sets a flag that the footer surfaces (see §Footer UX).

**Safety / privacy**
- **Fail-silent:** offline, timeout, non-200, or parse error → no notice, no log spam, no
  crash. The check never blocks or degrades the session.
- **Opt-out:** `KONTUR_NO_UPDATE_CHECK=1` disables it entirely (no file, no network).
- This is the **only** outbound network call Kontur makes on its own behalf. It sends no
  code and no telemetry — a plain GET to a public API. This is consistent with the "code
  stays where the team already trusts it" non-goal; the check is advisory and opt-out.
- The upgrade hint text names `brew upgrade kontur` (the only supported install path in MVP).

---

## D. Peer-version awareness

The two seats already reject **protocol-incompatible** peers: `PROTOCOL_VERSION` is checked
at handshake and a mismatch is rejected with `"protocol mismatch — update kontur (server
vN, client vM)"`. That stays. This section adds awareness of the **softer** case: seats on
the *same* protocol but *different releases* (e.g. `0.1.0` vs `0.1.1`).

**Wire changes** (bump `PROTOCOL_VERSION` 8 → 9):
- `ClientMsg::Hello` gains `client_version: String` (serde-defaulted to `""`).
- `ServerMsg::Welcome` gains `server_version: String` (serde-defaulted to `""`).

Both carry `CARGO_PKG_VERSION`. The server records each connected client's version; the
client records the server's version. The differing-version condition is surfaced into the
shared view state so the TUI footer can render the nudge on both seats.

**This is advisory only.** It never affects verdict eligibility, the four-eyes hold, or gate
parking. True wire incompatibility is already blocked by the protocol gate; a same-protocol
release skew is just a nudge to align.

---

## Footer UX (both seats)

A single reserved **DIM** footer line, rendered on host *and* operator, showing **at most
one** message, in priority order:

1. **Peer-version mismatch** (higher priority — it concerns the live session):
   `peer v0.1.1 · you v0.1.0 — align versions`
2. Else **upgrade available**:
   `v0.2.0 available — brew upgrade kontur`
3. Else nothing (the row collapses / stays blank).

The line is never loud and never uses an alarm color. Emphasis remains reserved for gates
that need a human — this is calm, decision-relevant status, nothing more. On small screens
it follows the same drop behaviour as the existing host-only footer.

---

## E. Version source of truth

Move the version into `[workspace.package]` in the root `Cargo.toml`; each crate sets
`version.workspace = true`. One bump covers all four crates, and the release pre-flight can
compare the tag against a single authoritative version. The `kontur` binary continues to
report `CARGO_PKG_VERSION`, which now resolves to the workspace version.

---

## Out of scope (recorded as future work)

- **Apple notarization / code-signing** — brew strips quarantine, so unsigned works via
  brew today; notarize later if we ship non-brew binaries.
- **Windows** — already unsupported per platform scope.
- **`curl | sh` installer** and **`cargo install` docs** — Releases make them possible; not
  built now.
- **Auto-self-update** — Kontur *notifies*; upgrading stays an explicit `brew upgrade`.
  (No silent binary replacement of a security-sensitive tool.)
- **homebrew-core submission** — premature for v0.1; would enable a bare `brew install
  kontur` with no tap step.
- **Linux arm64 release target** — added on demand.

---

## Testing

- **Release pipeline:** dry-run the workflow on a throwaway pre-release tag; verify all
  tarballs + checksums attach and the formula-bump commit lands in the tap. Verify the
  tag ↔ version guard fails a deliberately mistagged run.
- **Formula:** `brew install --build-from-source`-style audit (`brew audit --new kontur`,
  `brew test kontur`) in the tap repo; a clean-machine `brew install` + `kontur --version`.
- **Upgrade check:** unit tests for semver comparison and cache freshness; injected-clock
  and injected-HTTP tests for the fail-silent paths (offline, timeout, non-200, garbage
  body) asserting no notice and no error surfaces; test that `KONTUR_NO_UPDATE_CHECK=1`
  short-circuits before any file/network access.
- **Peer version:** codec round-trip tests for the new Hello/Welcome fields; a
  same-protocol / different-release handshake test asserting the footer nudge appears and
  that gates still park correctly (no behavioural change); confirm a pre-9 client is
  rejected by the existing protocol gate.

---

## Docs that move with this change

- **README** — install section (tap-first primary, one-liner secondary) + `brew upgrade`.
- **UX-kontur.md** — the footer version line (placement, priority, calm styling).
- **PRD-coop-supervisor.md** — the update check (privacy/opt-out) and the peer-version
  handshake fields; `PROTOCOL_VERSION` → 9.
- **CLAUDE.md** — status line noting Homebrew distribution + version awareness shipped.
