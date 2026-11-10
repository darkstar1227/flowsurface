# flowsurface — Codex Project Instructions

## Priority and scope

- Follow the user's latest instruction first; treat the current worktree as more
  current than stored notes.
- Reply in Traditional Chinese (Taiwan). Code, comments, and commit messages stay
  in English, matching the repo.
- `CLAUDE.md` holds the architecture overview; read it before non-trivial work.
- `origin` is the personal fork, `upstream` is `flowsurface-rs/flowsurface`.
  Never push to or open PRs against `upstream` unless the user asks.

## Commands

Toolchain is pinned to Rust 1.95.0 (`rust-toolchain.toml`). Package names differ
from directories: `flowsurface-data`, `flowsurface-exchange`,
`flowsurface-indicator-engine`.

```bash
cargo run
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p flowsurface-indicator-engine --locked
cargo test -p flowsurface --bin flowsurface --locked
cargo test -p flowsurface-data --lib --locked
cargo run --locked -p flowsurface-indicator-engine --bin flowsurface-indicator -- \
  test data/indicators/sources --namespace core
cargo test -p <package> <test_name_substring>   # single test
```

- Clippy runs with `-D warnings`; fix the code instead of adding `#[allow]`.
- When `.ind` sources or the indicator language change, run both the
  `flowsurface-data` tests and the CLI `test` smoke check.
- Filter long output in the command (`2>&1 | tail -40`).

## Agent routing

For anything beyond a short factual question, delegate to the matching custom
agent in `.claude/agents/` (Codex: reuse the same role split):

| Work | Role | Required outcome |
| --- | --- | --- |
| Locate code, callers, blast radius | `explorer` | Read-only evidence with `file:line`. |
| Implementation | `executor` | Minimal scoped change; fmt + clippy + scoped tests green. |
| Independent check | `verifier` | `CONFIRMED` / `REFUTED` / `INCONCLUSIVE` with evidence. |
| Network, credentials, proxy, TLS, catalog download | `security-review` | Findings with exploit scenario and severity. |
| Docs or an explicitly requested commit | `tech-writer` | Scoped commit or synchronized docs. |
| Multiple reports | `summarizer` | One conclusion; keep disagreements visible. |

Prefer CodeGraph (`codegraph explore "<question>"`) before grep/read loops.

## Worktrees, verify, commit, merge

1. Create a worktree before writing code:
   `git worktree add .claude/worktrees/<slug> -b <branch> main`.
2. Implement inside the worktree, never on the `main` checkout.
3. Verify (fmt, clippy, relevant tests). Do not commit or merge on failure.
4. Commit with explicit paths only (`git add -- <paths>`, never `-A` or `.`).
   Message style follows upstream: lowercase imperative summary, optional
   ` - ` separated details. No Conventional Commit prefixes.
5. Leave the worktree, merge into `main` from the main checkout, then remove
   the worktree and delete the branch with `-d`.
6. Push only when the user asks, and only to `origin`.

Docs-only edits and single-line typo fixes may skip the worktree.
