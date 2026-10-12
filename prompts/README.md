---
title: "Prompts"
kind: prompt-hub
corpus: blackbox-prompts
topic:
  - prompts
brief: "Map of checked-in interactive and dispatched-agent prompts: prose an operator points an agent at, or a brofile points a dispatched bro at. Distinct from system-defaults/ (installable artifacts) and agent skills (procedures a provider loads on its own)."
---

# Prompts

Checked-in **prose prompts** — documents whose audience is an agent, not a
human reader. Two species live here:

- **Operator-pointed** — a human aims a live agent at the file ("read
  `DESIGN_PANEL.md` and plan the phase in `design/x.md`"). These live at the root of
  `prompts/`.
- **Dispatched-agent lenses** — a brofile or orchestrator points a *dispatched*
  bro at the file as its operating doc. These live under
  [`agents/`](agents/README.md) so a lens can be tweaked without editing the
  brofile that references it.

This is **not** [`system-defaults/`](../system-defaults/system-defaults.md)
(installable artifacts: brofiles and global skills) and it is **not** an
[agent skill](../docs/agent-skills.md), which a provider lists and loads on its
own when the task matches. A prompt here is plain Markdown an agent is told to
read. A recurring procedure that agents should reach without being pointed at
it belongs in a skill; the session retro is the `bbox-retro` skill.

## Operator-Pointed Prompts

| Prompt | When to point an agent at it |
|--------|------------------------------|
| [daily-boot-sequence.md](daily-boot-sequence.md) | Daily conductor: clean → survey residuals/net-news → process gaps → refine & process the TODO list → closeout. Sequences the other daily prompts; owns the TODO-refinement loop + closeout. |
| [daily-cleaning.md](daily-cleaning.md) | Start-of-day environment reset: sync main, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall the checkout-host satellites and CLIs, restart the collectors (gated), check the deployed daemon with `bbox_doctor`. |
| [daily-cleaning-beta.md](daily-cleaning-beta.md) | Beta-line sibling of daily-cleaning.md: same reset, but tracks `beta/blackbox-v2` as the integration branch (sync + landing checks against beta) instead of `main`. Linux hosts. |
| [daily-cleaning-beta-mac.md](daily-cleaning-beta-mac.md) | macOS sibling of daily-cleaning-beta.md: same beta reset, but F3 signs the installed binaries with `stablesign` and F4 restarts the collectors via `launchctl kickstart -k` against their per-user LaunchAgents. |
| [gap-processing.md](gap-processing.md) | Caller-owned grouping, bro validation and synthesis of gap records; preserve evidence and resolve only with operator authority. |
| [CLOSEOUT.md](CLOSEOUT.md) | Fold a worktree back into `main`: commit, ff-only merge, push, clean up. |
| [CLOSEOUT-beta.md](CLOSEOUT-beta.md) | Beta-line sibling of CLOSEOUT.md: fold a worktree into `beta/blackbox-v2` instead of `main`. |
| [DESIGN_PANEL.md](DESIGN_PANEL.md) | Produce a reviewed implementation plan for **one phase or slice**: author-critic default, three-author panel escalation, adjudicated repair loop, independent review bookend to exact PASS. Prototype-stage orchestration runbook; fan-out is operator-directed via child orchestrators. |
| [REFRESH_ALL_CLIS.md](REFRESH_ALL_CLIS.md) | Refresh the harness research corpus: fan the `MINE_CLI` lens over all CLI subjects at their latest versions, integrate, validate, commit. Dispatches bros pointed at [`agents/MINE_CLI.md`](agents/MINE_CLI.md). |

## Dispatched-Agent Lenses

See [`agents/`](agents/README.md). Lens prompts referenced by brofiles/orchestrators.

## Conventions

- Keep operator-pointed prompts self-contained: an agent reads exactly one file
  and knows what to do. Paths inside a prompt are **repo-root-relative**.
- Dispatched-agent lenses should be the *single source of truth* for a bro's
  behavior, so the brofile stays a thin pointer (`prompts/agents/<lens>.md`).
- Gap-filing prompts must reuse existing gap kinds (`mcp_surface`,
  `tooling`, `workflow`, `agent`, `docs_runbook`, `refactor_primitive`,
  `ontology`, `eval_coverage`); never coin a new kind.
