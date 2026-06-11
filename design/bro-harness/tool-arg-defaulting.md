---
title: "Tool-arg defaulting: host-bound context for dispatched sessions"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
tags: [tools, dispatch, cwd, defaults, surface-conformity, gap-16d79781]
brief: "A host-set default/pin table (additionalContext) that mechanically fills or enforces tool args the model elides or misremembers, plus the param-naming conformity pass (cwd canonical) and dispatch-param hardening that make it safe."
---

# Tool-arg defaulting: host-bound context for dispatched sessions

## 1. Problem

Models forget arguments the host already knows, and the failure is silent.
Evidence chain (2026-06-10, thread-36f3cced / gap-16d79781):

- An orchestrator dispatched `bro_exec` with `cwd:` instead of `project_dir:`;
  the unknown param was **silently dropped**, the harness defaulted to the
  daemon's process cwd (`$HOME` under launchd), and the dispatched bro wrote a
  file into the operator's home directory.
- Wave-7 forensics: bros instructed to work in worktrees silently edited the
  primary checkout because file tools resolve against launch cwd (original
  gap facet — since closed by host-owned worktree creation, a72a216, plus
  `project_dir → --cwd → ToolCx.root`).
- glm's websearch flail (gap-c21e34a3) and `unwrap_jsonish` in the packet
  compiler are the same class: models fumbling generic MCP arg shapes.

The ambient scope block already tells dispatched agents their pre-bound ids
(session, project, bro, thread) — as prose the model must re-type into args.
That is a transcription loop with a per-call error rate.

## 2. Mechanism: the default/pin table

Dispatch passes the harness an `additional_context` map (JSON, not a
delimited string; env fallback `BRO_HARNESS_TOOL_DEFAULTS` for the subprocess
path). Keys form a small grammar:

```
<flavor>:<tool-pattern>.<param> = <value>

default:mcp.bbox_note.project = /repo/x      # fill if absent
pin:*.project_dir             = /repo/wt-7   # enforce; mismatch is an error
```

- **`default`** — applied only when the model elides the param. Model-supplied
  values always win.
- **`pin`** — identity args (worktree/cwd, session bindings). A model-supplied
  value that disagrees with the pin is refused with an explanatory error, not
  silently overridden: a model passing a different worktree than the host
  created is confused, and the error is the teaching signal.
- **`<tool-pattern>`** — exact tool name, or a glob (`*`,
  `mcp.bbox_*`). Glob keys are the extension point; first-match-wins with
  exact-before-glob ordering.

Resolution happens at the single tool-dispatch choke point in the harness
(beside `ToolCx`; the table is a sibling of `session_env` — same
daemon-supplied trust model, never inherited by shell children).

## 3. Safety requirements (each is load-bearing)

1. **Per-(tool, param) opt-in; no blanket name matching.** Absence is
   sometimes semantics: `project: None` on `bbox_note`/`bbox_learn` means
   *global scope*. A flat `project=foo` applied everywhere silently converts
   intended-global writes to project-scoped. Glob keys are allowed but the
   *host* writes them deliberately; the harness never infers.
2. **Transcript truthfulness.** The model's tool_use block will not contain
   injected args, so the tool_result carries a rider:
   `defaults_applied: {project: "/repo/x"}` (and `pin_enforced` /
   `pin_conflict` for the pin flavor). Without this, every defaulted call is
   future debugging archaeology.
3. **Schema validation at session start.** Defaulting fails open (a stale key
   restores today's behavior), so rot is silent. The harness validates every
   table key against the loaded tool schemas at session start and warns
   loudly on unknown `(tool, param)` pairs.
4. **Precedence is fixed and boring.** default: model > table. pin: table
   wins by refusal, never by silent rewrite.

## 4. Surface conformity pass (prerequisite)

The same logical parameter is named differently per layer: `project_dir`
(MCP tool params, ~54 sites), `cwd` (bro-protocol dispatch DTOs — the
contract bottom), `--cwd`/`ToolCx.root` (harness), `cwd` (roster DTO).
A defaulting table keyed on param names cannot be sane while the names drift.

Decision: **`cwd` is canonical** (it is the contract-bottom name and the
honest semantics — the directory the session runs in; `project_dir` also
collides conceptually with the bbox stores' `project` identity field, which
is a different concept and keeps its name). Migration is alias-based, not
breaking:

- MCP dispatch param structs accept both: `cwd` canonical,
  `#[serde(alias = "project_dir")]` retained indefinitely.
- `#[serde(deny_unknown_fields)]` on dispatch param structs so a misspelled
  or unknown param is a loud schema error, not a silent drop (the exact
  failure that wrote into `$HOME`).
- Tool docs and prompts migrate to `cwd` opportunistically.

## 5. Non-goals / relationship to native bindings

- This is not a second tool surface; there is no catalog cost and no
  binding code to drift. Native harness bindings over the
  `bro-capabilities` trait boundary remain a possible later layer for the
  hot core (note/report/dispatch ergonomics, smaller schemas); if built,
  they become sugar over this same table rather than a parallel mechanism.
- No model-facing setter. The table is host-set at dispatch (fleet TUI,
  workflow ops, `bro_exec`), period. In-session retargeting stays dead
  (a72a216).
- File-capable dispatches without an explicit cwd should fail closed (or
  land in a neutral scratch dir) rather than inherit the daemon's process
  cwd; daemon-cwd requires explicit opt-in. (Second facet of
  gap-16d79781.)

## 6. Implementation order

1. Dispatch-param hardening: `deny_unknown_fields` + `cwd` alias across
   `src/tools/bro_params.rs` / dispatch param structs (closes the
   silent-drop facet).
2. Default-cwd fail-closed policy for file-capable dispatches (closes the
   `$HOME` facet).
3. Harness: `additional_context` plumb (in-process arg + `--additional-context`
   + env fallback), table parse + session-start schema validation.
4. Choke-point application (default flavor + riders), then pin flavor.
5. Daemon dispatch surfaces populate the table mechanically: `cwd`/worktree
   pins from fleet/workflow worktree creation; ambient ids (session, bro,
   thread, project) as defaults for the bbox coordination tools.

## 7. Status note (2026-06-11): step-5 worktree pins are live

Step 5's worktree-pin half is implemented (gap-8144b4b5). Every daemon
dispatch path that carries the table — `bro_exec`/`bro_resume`, agent
dispatch, the workflow executor's fresh *and* resume turns (the resume branch
previously routed through the legacy `spawn_task` wrapper and silently
dropped the table), fleet cockpit via the control plane — now emits, from
`AmbientContext::tool_arg_defaults()` (src/orchestration/mod.rs):

- `default:mcp.bbox_note.session_id=<session>` (pre-existing), and
- `pin:*.project_dir=<canonical worktree root>` when the dispatch cwd is a
  daemon-managed worktree.

Detection is **one mechanical choke point** (`worktree_pin_target`), not
per-site emission at each worktree-creation surface, so fleet, agent, and
workflow dispatches can't drift apart. Two structural signals: (1) cwd under
a cockpit-managed parent (`bro_home/{fleet,agent}/worktrees`, the
`managed_worktrees` helpers); (2) cwd inside a *linked* git worktree
(nearest `.git` marker walking up is a file pointing into
`<base>/.git/worktrees/<name>`). Signal (2) is what covers workflow
`WorktreeCreate` worktrees — they land at arbitrary operator-chosen
`args.path` locations a root-prefix check cannot see, but every
daemon-created worktree is structurally a linked worktree. A plain repo
checkout (`.git` *directory*) never pins. Operator-created linked worktrees
also match; accepted deliberately — a dispatch confined to a worktree wants
the same confinement semantics regardless of who ran `git worktree add`.
The `pin:*.project_dir` glob is safe per §3.1: the project-scoped
coordination tools (notes/knowledge) take `project`, not `project_dir`; a
schema-drift tripwire test pins that assumption.

Still pending from step 5: ambient-id *defaults* beyond
`bbox_note.session_id` (e.g. `project` defaults for the coordination tools)
remain decision-gated under gap-ae22a6b2 — absence of `project` means global
scope (§3.1) and must not be mechanically filled without that decision.
