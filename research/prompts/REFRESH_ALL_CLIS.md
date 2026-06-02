---
title: "REFRESH_ALL_CLIS — orchestrate a full corpus refresh"
kind: research-prompt
corpus: blackbox-research
track: harness
topic:
  - harness
  - prompt
  - orchestration
brief: "Operating prompt for a top-level orchestrator (a main-loop agent like Claude) to refresh every CLI subject in the harness research corpus by fanning out MINE_CLI.md over operator-specified latest versions. Bros are pointed AT the prompt doc (they read MINE_CLI.md) rather than carrying baked-in lenses, so the procedure stays tweakable in one place. Encodes the hard-won bro dispatch mechanics (no tool-filter, deferred tools, fan-in, prune, scoped commit)."
---

# REFRESH_ALL_CLIS — orchestrate a full corpus refresh

You are the **harness-research orchestrator** (a main-loop agent). You refresh
the CLI subjects in `research/harness/` to the latest versions by fanning out
`research/prompts/MINE_CLI.md` across bros. This is **forward** refresh (known
axes); to expand the axis set, use `research/prompts/CLI_INVESTIGATOR.md`.

## Step 1 — inputs / version detection

The operator specifies, per CLI: `{subject, version, source, config}`. If
versions are omitted, detect installed ones:

- `claude` — `claude --version`; binaries under `~/.local/share/claude/versions/`.
- `codex` — `codex --version`; source at `~/repos/codex`.
- `antigravity` (`agy`) — `agy --version`; binary `~/.local/bin/agy`, config
  `~/.gemini`; docs repo `~/repos/antigravity-cli`.
- `vibe` — `vibe --version`; source at `~/repos/mistral-vibe`.

Confirm the latest version per subject and whether each is **source** or
**binary** (sets the mining method in MINE_CLI).

## Step 2 — dispatch (the key pattern)

For each subject, dispatch a bro that **reads `MINE_CLI.md` and executes it** —
**do not** bake the procedure into a bro lens. The dispatch prompt is minimal:

> Read `research/prompts/MINE_CLI.md` in full and follow it exactly for
> `SUBJECT=<x> VERSION=<y> SOURCE=<repo-path-or-binary> CONFIG=<dir>`. Honor its
> output contract (write the cells, or emit them as fenced blocks if you lack a
> write tool).

Keeping the procedure in the doc (single source of truth) is the whole point:
tweak `MINE_CLI.md`, every future refresh inherits it.

## Step 3 — dispatch mechanics (hard-won; do not relearn these)

- `bro_exec` with `provider: glm` (`glm-5.1`) or `deepseek` (`deepseek-v4-pro`)
  — both are capable source readers. Balance across providers to avoid one
  account's rate limits.
- **NO `allow_tools` filter.** bro-harness's tool names differ from Claude's
  (`Read`/`Bash`/`Grep` won't match), so allow-listing them **starves** the bro
  (observed: "I don't have shell"). The recursion guard + bro-harness's lack of a
  built-in Edit/Write already bound the surface. Add `disallow_tools` only if you
  have a specific reason.
- `project_dir` = the **source repo** (source mine) or `/home/invidious`
  (binary mine that needs `~/.local` + `~/.gemini` + `~/repos/<docs>`).
- `pin_model`, `pin_effort: high`.
- **Split a large CLI into 2–3 bros** by axis cluster (wire+loop / tool surfaces /
  governance) to keep each bounded; one bro per small subject is fine.
- Tell bros to `bro_report` at start and ~50%.
- Record every `taskId`.

## Step 4 — fan in

`bro_when_all(task_ids=[…], timeout_seconds=600)`. GLM ~5–10 min, deepseek
faster. If a task `timed_out`, call `bro_status(task_id, tail=N)` to check
tool-progress **before** re-waiting or cancelling — a timeout is not death. The
`result` field of a completed task carries the bro's findings.

## Step 5 — write & integrate

Default to **orchestrator-writes** for controlled vault edits: synthesize each
bro's findings into the subject's cells yourself (the bros are read-only miners).
(If you instead let bros write, each writes only its own subject's folder, so
parallel writes don't collide.) Then update snapshots + axis convergence tables;
the new-version snapshot `supersedes:` the prior.

## Step 6 — validate

Check frontmatter (`kind` ∈ {research-hub, research-axis, research-subject,
research-finding, research-prompt}; `corpus: blackbox-research`), relative-link
integrity, and a complete subject × axis matrix. Fix breaks — e.g. on a subject
rename, repoint the convergence-table cell links in every axis doc.

## Step 7 — cleanup & land

- `bro_prune` the `task_id`s **you** created (terminal only).
- Commit **scoped to your files by explicit path** (`research/`, plus any
  snapshot/axis edits) — **never `git add -A`** in this multi-tenant repo.
- Push `main` (fast-forward; if rejected, `git fetch` + rebase your commit +
  push).
- No Claude `Co-Authored-By` trailer (house rule).

## Step 8 — report

Per-subject version deltas, axes that changed, residuals, and any **new-axis
candidates** that surfaced — route those to `CLI_INVESTIGATOR.md`.

## Caveats

- Installing/refreshing a CLI mutates the host (PATH, telemetry, background
  self-update). Get operator approval before installing.
- This refresh maps onto **known** axes only. New capabilities that fit no axis
  go to the investigator, not into an ill-fitting cell.
