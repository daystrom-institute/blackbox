---
title: "Daily Boot Sequence"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - boot
brief: "How a live interactive agent runs an operator's daily TODO list: ingest the list, action items one at a time interactively (ground → reflect → decide → action → report), then debrief and self-update this doc. NOT a morning brief (bbox_inbox) — this is the operating protocol for a TODO-processing session, plus the ambient host/tool context future-you needs so it doesn't re-derive the environment each morning."
---

# Daily Boot Sequence

The operator opens a session by pasting a TODO list and says, in effect: *"walk
through this one-by-one; ask me to elaborate on each; we expand/refine/clarify/
action; at the end, debrief and update this doc."* This file is the protocol for
that session plus the environment context that makes it fast.

This is distinct from a **morning brief** (`bbox_inbox` — "what needs
attention") and from **daily-cleaning** (`prompts/daily-cleaning.md` —
environment reset). Either can be an optional prelude; this doc is the main loop.

## The session contract

- **One item at a time, in order.** Do not batch or race ahead. Finish the
  current item (or explicitly defer it) before opening the next.
- **Ground before you ask.** Cheap probes first (grep/find/Read/git, `bbox_gaps`,
  `bbox_thread_list`) so your clarifying questions are sharp and specific, never
  generic. The operator notices the difference.
- **Reflect, then surface genuine forks with framing.** State what you
  understand, name the real decisions and their tradeoffs, recommend one. Don't
  present a bare menu.
- **Action at the agreed scope, then report tersely.** Move on.

### Per-item loop

1. **Ground** the item in real repo/host state (don't trust memory or training
   priors — this repo overrides defaults via CLAUDE.md/PROJECT.md and bbox memory).
2. **Reflect** your understanding back; flag what's ambiguous.
3. **Decide** with the operator. Use `AskUserQuestion` for clean forks, prose for
   discussion. **Keep AUQ option labels short and self-contained — the operator's
   client truncates long labels and descriptions.** Put the full option text in
   your prose message before the AUQ. Treat "Other"/"No preference" as *withheld
   input*, not approval to pick the recommendation.
4. **Action** exactly the agreed scope.
5. **Report** what landed in one or two lines; note any deferral and where it's
   tracked.

## The operator's default posture (observed)

This operator consistently chooses **prep over execution** in a planning session,
and wants durable handoffs so work resumes cold:

- **Defer heavy/destructive execution.** Live agent dispatch, service restarts,
  and big mining passes get deferred — author the artifacts now, run later.
- **`bbox_thread` (kind `work_item`) is the deferral vehicle.** When an item is
  real implementation work, open a thread with a *rich* handoff body: `file:line`
  anchors, the non-obvious crux, decisions deferred to impl time, invariants, and
  the validation requirement. Set `handoff_doc` to the design doc. **List before
  create** (`bbox_thread_list`).
- **"Charter + stub" for new corpora.** Author the corpus map + domain charter +
  stub leaves with real source pointers; defer the mining.
- **"Prompts + thin brofiles, defer the live run" for orchestration.** Heavy
  logic in `prompts/`; brofiles stay thin pointers.
- **"Update the doc + open a thread" for "prepare for dispatch."** Reconcile the
  design doc to current code (verify anchors, fix drift, note lifecycle), then
  open the dispatch-handle thread.
- **Propose, never auto-execute, destructive cleanup.** Gap resolution, deletions,
  bulk ops are operator-gated, one scope at a time.

## Ambient environment (so you don't re-derive it)

- **Repo is multi-tenant.** Multiple Claude accounts + background bros mutate the
  working tree, `.bbox/` state, brofiles, and source *mid-session*. Only stage/
  commit/edit files **this session** changed; never revert peer files. (Seen live:
  `prompts/REFRESH_ALL_CLIS.md` + `prompts/agents/MINE_CLI.md` appeared mid-session.)
- **Shell:** zsh. `rtk` hook rewrites commands transparently. `fd` is absent and
  `rg` behaves inconsistently here — prefer `grep -rIn`, `find`, and the dedicated
  Read/Grep/Glob tools. Quote globs in command flags (`--include='*.rs'`).
- **`cp` is interactive-aliased** → use `command cp -af` in scripts or it silently
  skips existing targets.
- **Git remote:** `origin = git@github.com:daystrom-institute/blackbox.git`. Local-
  first dev; releases are manual/changelog-first (no PR flow).
- **Daemon:** prod `~/.local/bin/blackboxd` + `blackbox.service` (port 7264) is a
  **shared service** — ask before restarting (other accounts depend on it). Dev is
  `~/.local/bin/blackboxd-dev` + `blackbox-dev.service` (7265).
- **Build/install:** `cargo build --release` (root bins: blackboxd, bro, bro-irc,
  bro-slack) + `cargo build --release -p bro-harness`; then
  `install -m 755 target/release/{blackboxd,bro,bro-harness} ~/.local/bin/…`.
- **Worktrees:** fleet-managed under `~/.local/state/blackbox/bro/fleet/worktrees/`
  (out of scope for cleanup); main `target/` is tens of GB; `CARGO_TARGET_DIR`
  unset (ad-hoc worktrees cold-compile their own `target/`).

## Where things go (the corpus map)

| Need | Home | Notes |
|------|------|-------|
| Operator-pointed prose prompt | `prompts/*.md` | a human points an agent at it. Index: `prompts/README.md`. |
| Dispatched-bro lens | `prompts/agents/*.md` | a brofile points a bro at it; keep brofiles thin. |
| Intent (what we'll build) | `design/` | `kind: design`, `corpus: blackbox-design`, lifecycle proposed/partial/archived. |
| Description (what the world does) | `research/` | `kind: research-*`, `corpus: blackbox-research`, status stub→…→verified. |
| Canon (what it should be/do) | `specs/` | `kind: spec*`, `corpus: blackbox-spec`, status draft→specified→ratified, source-tier grading. |
| Brofile | `.bbox/brofiles/<name>.json` | `{name,version,provider,model,effort,context,lens,filters{allow,disallow}}`. |
| Gap evidence | `.bbox/gaps/*.json` | **don't rewrite** — historical provenance. |

## Toolset (what you reach for)

- **Grounding:** `grep -rIn`/`find`/Read; `git log`/`git grep`; `bbox_gaps`
  (active queue), `bbox_thread_list` (list-before-create), `bbox_knowledge` /
  `bbox_hybrid_search` (prior decisions — query before answering from priors).
- **Deferred MCP tools:** the `bbox_*` / `bro_*` tools are deferred; load their
  schemas with `ToolSearch("select:<tool>,<tool>")` before calling.
- **Deferral & coordination:** `bbox_thread(action=open, kind=work_item,
  handoff_doc=…)`; `bbox_gap_resolve` only on operator approval, one gap at a time.
- **Orchestration:** `bro_exec` launches a fresh bro and **carries
  `allow_recursion`** (set true for an orchestrator that dispatches sub-bros);
  `bro_agent_dispatch` hardcodes `allow_recursion=false` (leaves only);
  `bro_resume(session_id, provider)` resumes a specific agent; `bro_when_all` for
  fan-in. `bro_status(task_id, tail=N)` before declaring anything stuck.
- **Memory discipline:** durable memories are **operator-gated**. If the session
  produced a standing rule, present the verbatim text and wait for approval before
  `bbox_learn`/`bbox_remember`/`bbox_decide`. Task-local notes are fine.

## Closing ritual (debrief + self-update)

At the end of the session:

1. **Debrief:** list each item's deliverable and where it's tracked (thread ids,
   new files, specs). Name anything deferred and its handle.
2. **Candidate durable memories:** surface any standing rules the session implied,
   verbatim, for operator approval — do not persist unprompted.
3. **Self-update THIS doc:** fold in any new ambient fact, tool, or operator-
   posture observation you learned, so tomorrow's boot is faster. This file is
   meant to compound across sessions.
