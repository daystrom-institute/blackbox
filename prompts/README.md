---
title: "Prompts"
kind: prompt-hub
corpus: blackbox-prompts
topic:
  - prompts
brief: "Map of checked-in interactive and dispatched-agent prompts: prose an operator points an agent at, or a brofile points a dispatched bro at. Distinct from system-defaults/ (installable artifacts) and .claude skills (harness-native commands)."
---

# Prompts

Checked-in **prose prompts** — documents whose audience is an agent, not a
human reader. Two species live here:

- **Operator-pointed** — a human aims a live agent at the file ("read
  `DOC_REVIEW.md` and run it against `design/x.md`"). These live at the root of
  `prompts/`.
- **Dispatched-agent lenses** — a brofile or orchestrator points a *dispatched*
  bro at the file as its operating doc. These live under
  [`agents/`](agents/README.md) so a lens can be tweaked without editing the
  brofile that references it.

This is **not** [`system-defaults/`](../system-defaults/system-defaults.md)
(installable JSON artifacts — brofiles, workflows, packets, teams) and it is
**not** a `.claude` skill (harness-native slash commands). It is plain Markdown
an agent is told to read.

## Operator-Pointed Prompts

| Prompt | When to point an agent at it |
|--------|------------------------------|
| [daily-boot-sequence.md](daily-boot-sequence.md) | Daily conductor: clean → survey residuals/net-news → process gaps → refine & process the TODO list → closeout. Sequences the other daily prompts; owns the TODO-refinement loop + closeout. |
| [dogfood-orchestration.md](dogfood-orchestration.md) | Three-layer track-based work loop: a top-level interactive orchestrator owns 1..n tracks, dispatches one driver bro per track to drive `bro fleet` and surface the pain, and negotiates each tranche WITH the drivers (they feel the friction; the orchestrator de-dupes/synthesizes across tracks). Driver lens: [agents/DOGFOOD_DRIVER.md](agents/DOGFOOD_DRIVER.md). |
| [daily-cleaning.md](daily-cleaning.md) | Start-of-day environment reset: sync main, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall prod daemon/bro/bro-harness, restart prod service (gated). |
| [daily-cleaning-beta.md](daily-cleaning-beta.md) | Beta-line sibling of daily-cleaning.md: same reset, but tracks `beta/blackbox-v2` as the integration branch (sync + landing checks against beta) instead of `main`. Linux/systemd hosts. |
| [daily-cleaning-beta-mac.md](daily-cleaning-beta-mac.md) | macOS sibling of daily-cleaning-beta.md: same beta reset, but F4 restarts the prod daemon via `launchctl kickstart -k` against `~/Library/LaunchAgents/com.daystrom.blackbox.plist` (LaunchAgent, not systemd unit). |
| [gap-processing.md](gap-processing.md) | Launch the gap-processing **workflow** (`bro_orchestrate_run`): Cluster (codex) → foreach `atom_invoke` validators (deepseek) → Sieve (codex). Present grouped/sorted action lists; resolve operator-gated one at a time. |
| [CLOSEOUT.md](CLOSEOUT.md) | Fold a worktree back into `main`: commit, ff-only merge, push, clean up. |
| [CLOSEOUT-beta.md](CLOSEOUT-beta.md) | Beta-line sibling of CLOSEOUT.md: fold a worktree into `beta/blackbox-v2` instead of `main`. |
| [DOC_REVIEW.md](DOC_REVIEW.md) | Dispatch the 5-lens `blackbox-review` ensemble against a design doc. |
| [RETRO_INTERACTIVE.md](RETRO_INTERACTIVE.md) | End-of-session retro for a **live interactive** agent (tools, MCP, instructions, operator steering). Files gaps + follow-up notes. |
| [RETRO_HARNESS.md](RETRO_HARNESS.md) | End-of-session self-report for a **`bro fleet` / bro-harness** session: what felt helpful, noisy, missing, or awkward. Files gaps only for reusable substrate defects. |
| [RETRO_ISOLATE_REFACTOR.md](RETRO_ISOLATE_REFACTOR.md) | Post-probe retro for a **code-mode session driving the refactor namespace bindings** (`code.*`/`lsp.*`/`analysis.*`/`edits.*`) — the live-probe instrument for refactor-tools-v2. Files gaps in `*/refactor-tools/*`. |
| [JAVA_REFACTOR_DELEGATION.md](JAVA_REFACTOR_DELEGATION.md) | Orchestrator playbook for **delegating Java structural refactoring** (god-class decomposition, extract-class) to a dispatched agent driving the code-mode refactor bindings: the flow, how to brief the agent, dispatch mechanics, footguns, and the verify loop. Pairs with [RETRO_ISOLATE_REFACTOR.md](RETRO_ISOLATE_REFACTOR.md). |
| [RUST_REFACTOR_DELEGATION.md](RUST_REFACTOR_DELEGATION.md) | Orchestrator playbook for **delegating Rust structural refactoring** to a dispatched agent driving `analysis.*`/`rust.*`/`lsp.*`/`edits.*` plus the compiler repair loop. Records Rust-specific import, span, rust-analyzer, call-site, and lift-to-free limitations. |
| [REFRESH_ALL_CLIS.md](REFRESH_ALL_CLIS.md) | Refresh the harness research corpus: fan the `MINE_CLI` lens over all CLI subjects at their latest versions, integrate, validate, commit. Dispatches bros pointed at [`agents/MINE_CLI.md`](agents/MINE_CLI.md). |

## Dispatched-Agent Lenses

See [`agents/`](agents/README.md). Lens prompts referenced by brofiles/orchestrators.

## Conventions

- Keep operator-pointed prompts self-contained: an agent reads exactly one file
  and knows what to do. Paths inside a prompt are **repo-root-relative**.
- Dispatched-agent lenses should be the *single source of truth* for a bro's
  behavior, so the brofile stays a thin pointer (`prompts/agents/<lens>.md`).
- Gap/note-filing prompts must reuse existing gap kinds (`mcp_surface`,
  `tooling`, `workflow`, `agent`, `docs_runbook`, `refactor_primitive`,
  `ontology`, `eval_coverage`, `packet_ast`) — never coin a new kind.
