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
| [daily-cleaning.md](daily-cleaning.md) | Start-of-day environment reset: sync main, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall prod daemon/bro/bro-harness, restart prod service (gated). |
| [gap-processing.md](gap-processing.md) | Launch the gap-processing **workflow** (`bro_orchestrate_run`): Cluster (codex) → foreach `atom_invoke` validators (deepseek) → Sieve (codex). Present grouped/sorted action lists; resolve operator-gated one at a time. |
| [CLOSEOUT.md](CLOSEOUT.md) | Fold a worktree back into `main`: commit, ff-only merge, push, clean up. |
| [DOC_REVIEW.md](DOC_REVIEW.md) | Dispatch the 5-lens `blackbox-review` ensemble against a design doc. |
| [RETRO_INTERACTIVE.md](RETRO_INTERACTIVE.md) | End-of-session retro for a **live interactive** agent (tools, MCP, instructions, operator steering). Files gaps + follow-up notes. |
| [RETRO_HARNESS.md](RETRO_HARNESS.md) | End-of-session retro for a **`bro fleet` / bro-harness** session (built-in tools, injected context, intern, turn machinery). Files gaps. |
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
