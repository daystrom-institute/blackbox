---
title: "Research Prompts"
kind: research-hub
corpus: blackbox-research
track: harness
topic:
  - harness
  - prompts
brief: "Hub for the harness-research operating prompts — the repeatable procedures that run the research program: forward mining of one CLI (MINE_CLI), orchestrated fan-out refresh of all CLIs (REFRESH_ALL_CLIS), and backward discovery of missing axes (CLI_INVESTIGATOR). These are the tweakable single-source-of-truth instructions; agents/bros are pointed at these docs rather than carrying baked-in lenses."
---

# Research Prompts

The operating procedures for the harness research program. Agents and dispatched
bros are pointed **at these docs** (they read them) rather than carrying the
procedure in a baked-in lens — so the procedure stays tweakable in one place.

## The three prompts

- **[MINE_CLI](MINE_CLI.md)** — *forward*, single CLI. Ground in the corpus, then
  map one CLI version onto the **existing** 15 axes, producing confidence-tagged
  cells + an updated snapshot. The per-version, reproducible mining procedure.
- **[REFRESH_ALL_CLIS](REFRESH_ALL_CLIS.md)** — *orchestrator*. Fans `MINE_CLI`
  out over the operator's latest CLI versions via bros, fans in, integrates,
  validates, prunes, and lands. Carries the hard-won bro dispatch mechanics.
- **[CLI_INVESTIGATOR](CLI_INVESTIGATOR.md)** — *backward*, single CLI. Works from
  source → taxonomy to uncover agent-facing dimensions the axes **miss**; outputs
  candidate new axes + extensions for the operator to fold into the charter.

## Forward vs backward

`MINE_CLI` works **forward** from the axes (fill known cells). `CLI_INVESTIGATOR`
works **backward** from the source (find what the axes don't capture). Run the
investigator when onboarding a new/odd harness or periodically to keep the axis
set honest; run mining (via REFRESH_ALL_CLIS) on every version bump. Both ground
in the charter: [`harness-tracks.md`](../harness/harness-tracks.md).
