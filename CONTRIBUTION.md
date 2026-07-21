# Contributing to Kontur (КОНТУР-1)

Thanks for helping build Kontur. This project lives and dies by one guarantee —
**nothing reaches `main` without independent human review** — so the way we take
in code deliberately mirrors that: every change is tracked, reviewed, and merged
only by a code owner.

Read `CLAUDE.md` and the design docs (`docs/PRD-coop-supervisor.md`,
`docs/UX-kontur.md`) before you start. They are the source of truth; if a change
contradicts a decision recorded there, **flag it — don't silently diverge.**

---

## The workflow at a glance

```
issue  →  branch  →  commits  →  pull request (references the issue)  →  review  →  code-owner merge
```

Every code change follows this path. No direct pushes to `main` — the branch is
protected, and only a code owner can merge.

---

## 1. Open an issue first

Every change starts as an issue. This is where the "why" is argued before the
"how" is written.

- **Search first.** Check [existing issues](../../issues) so you don't duplicate
  one.
- **Open a new issue** describing the problem or proposal. Include:
  - What's wrong / what's missing, and the impact.
  - For bugs: steps to reproduce, expected vs. actual, and your environment
    (OS, `cargo --version`, `rustc --version`).
  - For features: the user-facing behaviour and which requirement (FR-\*) or doc
    section it touches, if any.
- **Wait for a quick nod** on non-trivial or invariant-touching work before
  writing code. Anything that affects the two-signatory mechanism, the audit
  chain, signing keys, or gate logic **must** be discussed on the issue first —
  see [Security-sensitive changes](#security-sensitive-changes).

Note the issue number — you'll reference it from your branch and PR.

## 2. Create a branch

Branch off the latest `main`. Never work on `main` directly.

```sh
git fetch origin
git switch -c <type>/<issue-number>-<short-slug> origin/main
```

Branch naming — `<type>/<issue>-<slug>`:

| Type       | Use for                                  |
|------------|------------------------------------------|
| `feat`     | new capability                           |
| `fix`      | bug fix                                  |
| `docs`     | documentation only                       |
| `refactor` | no behaviour change                      |
| `test`     | tests only                               |
| `chore`    | tooling, deps, CI                        |

Example: `feat/142-sealed-verdict-audit` for issue #142.

## 3. Make your change

Keep it **small and single-concern** — the same discipline the product enforces
on agents. One issue, one focused branch.

- **Docs move with behaviour.** If you change behaviour the PRD or UX doc
  describes, update that doc in the *same* change.
- **Don't weaken the invariants.** The seven non-negotiable invariants in
  `CLAUDE.md` are the product. If a task seems to require breaking one, stop and
  raise it on the issue.
- **Match the surrounding code** — its naming, idioms, and comment density.

### Build, test, and lint before you push

All of these must pass locally:

```sh
cargo build                              # build everything
cargo test                               # whole-workspace tests
cargo clippy --all-targets -- -D warnings  # lint (warnings are errors)
cargo fmt --all                          # format
```

Add tests for new behaviour and bug fixes. Crypto, gate logic, and audit-chain
code warrant extra coverage.

## 4. Commit

Write clear, imperative commit messages. We follow a Conventional-Commits-style
subject:

```
<type>: <concise summary in the imperative mood>

Optional body explaining the why, not the what. Wrap at ~72 cols.

Refs #<issue-number>
```

Example:

```
fix: reject bare no-go without a remedy at the merge gate

A no-go with no steer prompt or hand-edit slipped through when the
remedy field was empty rather than absent. Validate presence, not
just parseability.

Refs #142
```

- Reference the issue in the body (`Refs #142`, or `Closes #142` if the commit
  fully resolves it).
- Keep commits logically scoped; rebase/squash noise before opening the PR.
- Don't commit secrets, keys, or generated artifacts. Never log signing keys.

## 5. Open a pull request

Push your branch and open a PR against `main`.

```sh
git push -u origin <your-branch>
```

The PR **must reference its issue** so the two stay linked:

- Put `Closes #<issue-number>` (or `Refs #<issue-number>` if it only partially
  addresses it) in the PR description. `Closes` auto-closes the issue on merge.
- Give the PR a clear title and a description that covers:
  - **What** changed and **why** (link the issue for the full context).
  - **How you tested it** — commands run, cases covered.
  - **Docs** — which docs you updated, or why none needed updating.
  - **Invariants** — confirm you haven't weakened any, or call out the ones you
    touched and how they stay intact.
- Keep the PR focused on its single concern. Split unrelated changes into
  separate issues/PRs.
- Mark it a **draft** if it isn't ready for review yet.

CI must be green. Reviewers won't merge a red PR.

## 6. Review and merge

This is where Kontur's own philosophy applies to Kontur itself.

- **A code owner reviews every PR.** Reviewers are listed in
  [`.github/CODEOWNERS`](.github/CODEOWNERS); GitHub requests them automatically.
- **Only a code owner can merge.** Branch protection on `main` enforces this —
  contributors cannot self-merge. This is the four-eyes principle applied to the
  repo: the maker (you) never merges your own change.
- **Address feedback** by pushing follow-up commits to the same branch; don't
  force-push over review history unless asked. Re-request review when ready.
- On approval, a code owner merges — squash-merge is preferred, keeping `main`'s
  history one reviewed commit per change, echoing how Kontur lands agent work.

Once merged, delete your branch. If your PR closed an issue, confirm the issue
closed as expected.

---

## Security-sensitive changes

Kontur handles operator **signing keys** and a **tamper-evident audit chain**.
The following are high-risk and get extra scrutiny — expect a slower, more
thorough review, and flag them clearly on the issue and PR:

- The two-signatory / four-eyes engine (`kontur-core`).
- Verdict independence, blind (sealed) review, no-bare-veto, hand-edit
  acceptance, park-on-disconnect.
- The audit chain: signing, hashing, record immutability.
- Anything touching key generation, storage, or the network handshake.

**Never** log secrets, weaken signature generation/verification, or make audit
records mutable. If you believe you've found a security vulnerability, do **not**
open a public issue — contact the maintainers privately first.

---

## Reporting bugs and requesting features

- **Bug:** open an issue with reproduction steps, expected vs. actual, and your
  environment.
- **Feature / design change:** open an issue describing the behaviour and the
  requirement or doc section it relates to. For anything that shifts a recorded
  decision, argue it on the issue before writing code — we flag conflicts, we
  don't relitigate them in a diff.

---

## Code of conduct

Be direct, be kind, keep it operational. Assume good faith, review as a
co-equal, and keep discussion about the code and the decision — not the person.

Thank you for contributing. Two keys, always.
