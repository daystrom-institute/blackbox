---
title: "Edit-Only Worktree"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic:
  - prompts
  - prompts-agents
brief: "Operating rules for an agent dispatched into a cold or edit-only checkout: commit granular work with tests written, never run compile-shaped gates locally; the orchestrator verifies lane-side and steers corrections back."
---

# Edit-Only Worktree

You are working in an EDIT-ONLY checkout: a git worktree or fresh clone
whose build cache is cold. On this estate the dev machine is the control
plane, not the compute tier. A cold cargo invocation here is a 20+ minute
full-dependency compile plus per-binary macOS assessment, stolen from every
sibling lane, and the repo blocks it mechanically
(`scripts/rustc-cold-guard.sh`).

## What you do

- Edit code and docs. Write tests for everything you change; do not run
  them.
- Format with `scripts/fmt.sh` (formatting only; it does not compile).
- Commit granularly with conventional-commit messages as you complete
  coherent units. Done-criteria is committed (and, only if your brief says
  so, pushed) work, not locally-verified work.
- Report what you changed, what you could not verify, and every assumption
  you made in place of a compile check.

## What you never do

- `cargo check` / `cargo build` / `cargo test` / `cargo nextest` /
  `cargo clippy`, or any other compile-shaped command. The cold-checkout
  guard fails these loudly by design. Do not set `BBOX_ALLOW_COLD_BUILD=1`;
  that override is an operator decision, not yours.
- Push, unless your brief explicitly says to.
- Touch files outside your assignment; the tree may be multi-tenant
  (stage and commit by explicit path only).

## Who verifies

The orchestrator runs all gates lane-side against your ref
(`lane-run.sh --ref`, `submit-bbox-verify.sh --ref`) and sends corrections
back to YOU, with your context intact, rather than dispatching a stranger.
Expect a follow-up; silence does not mean green.

## House rules that bind you regardless

- No em dashes in any written output (commits, comments, docs, prose).
- No AI attribution anywhere (no Co-Authored-By trailers, no
  generated-with lines).
- Tests you WRITE must honor the repo test-isolation invariants:
  canonicalize tempdir roots before path assertions; never touch real
  `$HOME`/XDG/prod state or the prod daemon; use `SharedState::for_test`
  for daemon-state tests.
