---
title: "Delegating Java Structural Refactoring"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - refactor-tools
  - orchestration
brief: "Orchestrator playbook for delegating Java structural refactoring (god-class decomposition, extract-class) to a dispatched coding agent that drives the code-mode refactor bindings (analysis.*/java.*/code.*/edits.*). Genericizes the live-probe briefs into a reusable delegation pattern: the flow, how to brief the agent, dispatch mechanics, footguns, and the orchestrator's own verify loop. Pairs with the post-run retro in prompts/RETRO_ISOLATE_REFACTOR.md."
---

# Delegating Java Structural Refactoring

You are an **orchestrator**. You are not going to drive the refactor tools
yourself — you are going to dispatch a coding agent that drives them inside
code-mode cells, then independently verify what it produced. This file is how
you brief that agent and run the loop.

Use it when the work is **large, mechanical structural moves on DI-heavy Java**
— decomposing a god class, extracting a cohesive concern into a delegate, moving
an injection point. It is overkill for a one-line rename or a move a human can
do in a single edit.

The tools and their invariants live in `crates/bro-harness/src/bindings/` (read
`crates/bro-harness/src/bindings/AGENTS.md` for the trust model). The design home
is `design/bro-harness/refactor-v2-pressure-test.md`.

## The toolbox the agent drives (don't re-explain it — point at it)

The agent runs in **code-mode (`--code-mode only`)** and composes these
namespaces in `exec` cells. Each carries its own runtime contract via a
`describe` call — your brief should *point the agent at the contracts*, not
restate them:

- **`analysis.cohesionClusters({ file })`** — the seam-finder. Partitions a
  class's methods into cohesive clusters (connector-aware: a high-fan-out shared
  field will not fuse distinct concerns) and returns each cluster extract-ready
  with `{ item_names, move_fields, name_hint, score, expected_wiring }`. This
  replaces the agent reconstructing cohesion by hand. `analysis.describe`.
- **`analysis.references({ symbols, kinds? })`** — the caller/reference survey.
  Returns compact per-symbol counts, workspace-relative files, production/test
  split, and capped examples without materializing full capture payloads. Use it
  before extraction to decide whether `wrappers: true` is needed, and for cheap
  blast-radius checks. `analysis.describe`.
- **`analysis.methodRegions({ file, method, className?, ranges? })`** — the
  long-method region gate. Returns filtered top-level statement regions by
  default, optional nested statement regions, and optional candidate-range facts:
  captures, live-outs, field touches, lambdas/listeners, non-local control flow,
  and `extractability.stop_reasons`.
  For giant methods, use `statementContains` / `statementStartLine` /
  `statementEndLine` / `statementLimit`; if the target marker sits inside one
  giant loop/block, add `includeNestedStatementRegions:true` to get inner
  statement line ranges. Set `includeStatementRegions:false` for gate-only
  calls. Captures/live-outs declared with Java `var` keep `type: "var"` and add
  `resolved_type` when syntax-only inference can prove a concrete type.
  Live-outs carry `after_use_kinds` and `component_tree_consumptions` so UI tree
  wiring and return-value outputs are visible. Use this before extract-method
  work; do not reconstruct live-outs by hand.
- **`java.extractClass({ file, target, delegateField, methods, moveFields?,
  wrappers?, previewOnly? })`** — moves methods + fields into a new delegate
  class, synthesizes both sides, returns `{ changes, creates, findings,
  dependency_projection }` for the edits algebra. `previewOnly` runs the same
  planner but omits heavy edit payloads so a risky seam can be inspected before
  applying. `java.describe({ transform: "extractClass" })`.
- **`java.extractMethodCodeBlock({ file, oldText, methodName, className? })`**
  — extracts one exact contiguous Java code block into a helper method. Run
  `analysis.methodRegions` first; if the candidate is non-contiguous, has
  multiple live-outs, mutates captures, or crosses return/break/continue, stop
  and report the missing construct rather than hand-editing a helper.
- **`java.removeUnusedConstructorParams({ file })`** — drops dead `@Inject`
  constructor params left after an extract strands a dependency (moves the
  injection point). Returns `{ changes, ... }`. `java.describe`.
- **`code.*`** — facts (items, query, read) for surveying callers and inventory.
- **`edits.*`** — the one mutation path: `begin → merge/createFile → apply`.
  `apply` is the only write; it validates with tree-sitter and bounces with
  repairable findings.

The contracts are the source of truth. If you find yourself pasting tool
mechanics into the brief, stop and point at `*.describe` instead.

## The canonical decomposition flow

This is the chain the agent should run for "extract one cohesive concern from a
god class":

1. `analysis.cohesionClusters({ file })` → pick the **cleanest real seam**:
   high `score`, more than one method, `expected_wiring === "delegate"`.
2. Survey callers with
   `analysis.references({ symbols: seam.item_names, kinds: ["method_invocation"] })`;
   if anything outside the file calls a moved method, pass `wrappers: true`.
3. Decide whether this is a clean seam or a risky seam. Risky means unclear
   captured dependencies, mutable state ownership, callback/source-instance
   topology, or a payload you want to inspect before carrying through the
   isolate. For risky seams, call `java.extractClass({ ..., previewOnly: true })`
   first and inspect `dependency_projection`; re-call without `previewOnly` only
   if it is safe. Clean seams skip preview.
4. `java.extractClass({ ... })` — **leave `wiring` unset**. It auto-selects:
   a Guice/DI-managed source (uses `@Inject`) gets `external_injection`, so the
   delegate is a container-constructed `@Inject` bean and stays interceptable by
   Guice AOP. (`own_construction` `new`s up the delegate — invisible to Guice
   method interception. Only force it when AOP is irrelevant, e.g. AspectJ
   weaving or a non-DI source.)
5. For the real mutation, keep `java.extractClass → edits.begin →
   createFile(s) → merge(changes) → edits.apply` in **one exec cell** with a
   local `result` variable. Do not split the heavy edit payload across cells
   with `store()`/`load()`. If the real call's `dependency_projection` contains
   non-injectable params, the same cell should stop before `edits.apply` and
   report the projection. A clean service seam should have injectable/provider
   captures or no captures.
6. **Compile-gate** with the project's incremental build (the post-apply truth
   that tree-sitter validation cannot give you).
7. Run post-apply hygiene on touched/created files only. Use
   `java.hygiene({ files })` for routine import/spacing cleanup, or the narrower
   `java.organizeImports({ files })` / `java.normalizeWhitespace({ files })`
   when you need one half. Do not reimplement cleanup inside each semantic
   transform. `java.removeUnusedConstructorParams({ file })` runs
   **after** the extract is applied — the orphaned `this.dep = dep` must already
   be gone for the param to read as unused. Re-compile if hygiene changes
anything.

## Monolithic method extraction flow

For "extract one stage from a giant method", use this smaller flow instead of
the class-decomposition flow:

1. For giant methods, search/filter first instead of dumping the full method:
   `analysis.methodRegions({ file, method, className, statementContains,
   statementLimit })`, or use a line window with `statementStartLine` /
   `statementEndLine`. If the returned candidate is only a giant enclosing
   loop/block, re-run with `includeNestedStatementRegions:true` and use the
   inner statement `line_range` values as markers for exact range gating. Pick
   one candidate by `line_range`/`preview`.
2. Re-run `analysis.methodRegions` with one exact candidate range. Treat the
   returned gates as authoritative:
   - `requested_contiguous` must be true for today's one-block extractor.
   - If `extractability.can_extract_with_current_tool` is true, proceed normally.
   - If the only blocker is multiple live-outs, inspect `after_use_kinds` and
     `component_tree_consumptions` before deciding whether the live-out set is a
     true result bundle or a UI tree / return-value boundary. For true top-level
     outputs with explicit Java types, call `java.extractMethodCodeBlock` with
     `resultRecord: true` (and usually `resultRecordName`). If a capture or
     live-out has `type: "var"` plus `resolved_type`, treat the resolved type as
     the explicit type for the helper boundary. If it has `type: "var"` without
     `resolved_type`, resolve it yourself and pass `parameters`/`arguments` or
     `returnType`/`returnVar`. Do not use the result-record path for unresolved
     `var`/inferred types, discontiguous ranges, or values declared inside a
     nested scope that would not be visible at the helper return site.
3. If gates pass, bounded-read the exact range and call
   `java.extractMethodCodeBlock({ file, oldText, methodName, className })`,
   adding `resultRecord: true` only for the safe bundle case above.
4. Apply via `edits.merge → edits.apply`, compile, `java.hygiene({ files:
   [file] })`, and compile again if hygiene changed anything.
5. Inspect the diff around the call site and inserted helper after hygiene.
   Compile success is necessary but not sufficient if helper spacing or
   indentation is obviously wrong.
6. If gates fail, stop with the `stop_reasons`, `live_outs`, and candidate
   ranges. Do not hand-build a record object or discontiguous helper inside a
   probe.

The load-bearing mutation cell shape:

```ts
const result = await java.extractClass(args);
const projection = result.dependency_projection;
if (projection.non_injectable_params?.length) {
  text(JSON.stringify({ stopped: true, dependency_projection: projection }, null, 2));
  throw new Error("stopped before apply: non-injectable captured dependency");
}
const es = await edits.begin();
for (const c of result.creates) await edits.createFile({ es, path: c.path, content: c.content });
await edits.merge({ es, changes: result.changes });
const applied = await edits.apply({ es });
text(JSON.stringify({ applied, dependency_projection: projection, fixme_count: result.fixme_count }, null, 2));
```

## Briefing the dispatched agent

A good brief is a **task with guardrails**, not a script. Tell it:

- The target file and the goal ("extract ONE cohesive concern into a delegate,
  cleanly"), and that the codebase is DI-managed (so the extract uses
  `external_injection` automatically).
- To consult `analysis.describe` and `java.describe` before first use.
- **Leave `wiring` unset** — it auto-selects. (Do not pass the cohesion
  `expected_wiring` value as `java.extractClass`'s `wiring`: they are different
  axes — cohesion *topology* vs DI *strategy*. This conflation is a real trap.)
- Use `wrappers` to preserve the public API; survey callers first.
- Use `previewOnly` only when the seam is genuinely risky or ambiguous: mixed
  mutable state, unclear captured dependencies, source-instance callbacks, or a
  large edit payload. Do not add a mandatory preview tax to clean service-only
  seams; the normal call already returns `dependency_projection`.
- The exact **compile-gate command** for the project.
- For noisy compile gates, give the agent the `shell_run` call with host-side
  `output_filter` as the primary invocation shape. It preserves the primary
  command exit status because filtering happens after capture. Report the
  unfiltered command as the gate even when rendered output is filtered. Filter
  streams accept either one regex string or an array of regex strings.
- That transforms are **not idempotent** — a target-exists refusal after a
  successful apply means that step is DONE, not a retry (without this, agents
  shell-delete the created file and loop).
- To **move the injection point** with `removeUnusedConstructorParams` after the
  extract compiles.
- To treat hygiene as a standard post-apply phase on touched files only:
  call `java.hygiene({ files })`, then compile again if hygiene changed
  anything. Do not ask the agent to hand-roll cosmetic cleanup with ad hoc
  string edits.
- **One concern per dispatch.** Decomposing a god class is many extractions;
  each is its own dispatch against a fresh seam survey.
- A **structured return JSON** so you can verify without reading the transcript:
  `{ concern, cluster_score, methods_moved, fields_moved, preview_used,
  dependency_projection, delegate_is_inject, injection_point_moved,
  params_removed, applied, compile, cells_used, summary }`.

## Dispatch mechanics

- **Model matters.** Structural refactoring has a reasoning floor — a weak model
  hand-builds wrappers and grinds dozens of cells; a capable model takes the
  scored seam straight from `cohesionClusters` and uses the levers. Dispatch a
  capable model and keep a strong fallback for hard analysis.
- **`--code-mode only`** so the bindings are the authorial surface.
- **Isolated throwaway worktree.** Dispatch against a fresh detached worktree of
  the target repo (`git worktree add --detach /tmp/<scratch> <ref>`), never the
  real checkout — probes must never mutate working state. Build the harness from
  the current worktree so it carries the bindings under test.
- **Build env is not inherited.** Harness shell children don't inherit things
  like `JAVA_HOME`; pass them on the non-secret shell-env lane (the harness
  `--shell-env '{"JAVA_HOME":"..."}'`, or `project_dispatch.env` for daemon
  dispatch). Credentials ride a separate lane via provider env.
- **The compile-gate is the truth.** tree-sitter validation catches syntax, not
  semantics; only the real build confirms the extract. For `external_injection`,
  note the build proves *compilation* but not Guice *wireability* — a missing
  binding surfaces at injector creation, not `compile`. Clean service-only seams
  wire; a seam that drags in a non-injectable captured field will not.

## Footguns (hard-won — pre-empt them in the brief or the loop)

- **`wiring` left unset.** The single most important instruction. Setting it
  (especially to a cohesion-topology value) defeats the AOP-ready default.
- **`removeUnusedConstructorParams` runs after the extract apply**, never before.
- **Prefer `expected_wiring === "delegate"`, score-high, multi-method seams.**
  `source_instance` is bidirectional coupling (messy); singletons/low scores are
  weak or forwarded-field false seams.
- **Agents mis-report `cells_used`.** Read the real count from the session event
  log (`$BRO_HOME/harness-sessions/<id>.events.jsonl`), not the self-report.
- **external_injection needs every ctor param injectable.** Captured (read-not-
  moved) deps become the delegate's `@Inject` params; if one is non-injectable
  view state, the build compiles but Guice can't wire it. Pick clean
  service-cluster seams. `dependency_projection` now calls this out before you
  apply.
- **Do not pipe the build through `head`/`tail`/short-circuiting filters.** Use
  `shell_run({ output_filter })` when the build is noisy, or run the bare gate.
  The returned `exit_code` must be the build's exit code, not a filter's.
- **If a long code-mode `shell_run` yields while still running, resume the cell
  with `wait`.** Do not re-run the build just because the outer JS cell yielded.
- **Hygiene is taught as a routine, not hidden in every mutator.** The semantic
  transform should do the move; a separate post-apply hygiene phase handles
  imports/formatting through `java.hygiene({ files })`. Residual hygiene is a
  construct gap, not a reason to rerun the semantic transform.
- **Compile success does not prove generated helper quality.** Inspect the local
  diff after `java.extractMethodCodeBlock` + hygiene. The expected shape is a
  normally indented call site, one blank line between methods, helper signature
  on its own line, and moved-body relative indentation preserved.
- **Steers are recipe deltas.** If you have to steer a running probe, treat the
  steer as evidence that the recipe/prompt is under-specified. Before the next
  rerun, refine the prompt text or add a recipe comment so future agents start
  on the corrected path.

## Your verify loop (don't trust the self-report)

The dispatched agent's final JSON is a claim, not evidence. As orchestrator:

1. **Re-run the compile-gate yourself** on the worktree. Exit 0 or it didn't
   happen. If you filter output, use `shell_run`'s `output_filter`, not a shell
   pipeline, and keep the exact unfiltered command in the run record.
2. **Inspect the diff.** Is the delegate an `@Inject` bean (not `new`-ed)? Did
   the source get it injected? Did the injection point actually move (source ctor
   shrank), or are dead params left? Did mutable state move as plain fields, not
   bogus `@Inject` params?
3. **If there is friction, fix at the source, then re-dispatch.** Order of
   preference: the **binding** (behavior) > the **contract docs**
   (`*.describe` / namespace declarations — agents act on these cold, so a
   misleading recipe silently misdirects them) > a **gap note** in the
   `*/refactor-tools/*` dedupe namespace (`gap_kind: refactor_primitive` or
   `tooling`). Re-probe the same task and measure the delta.
4. **Run the retro.** On a clean completion, point a retro pass at
   `prompts/RETRO_ISOLATE_REFACTOR.md` to harvest binding-level friction.

This is the probe-driven loop: land a slice → dispatch a real agent at the task
→ verify independently → fix at the source → re-dispatch for the measurable
delta. The agent feels the friction; you turn it into substrate.

## Before any commit

This is a public repo. **Genericize client identifiers** out of every committed
artifact (commit messages, design notes, gap notes, code comments): no real
repo/class/field/package names from a client codebase. Describe shapes instead
("a ~3,700-line DI-managed view", "an admin-service cluster"). Probe labels
(`probe-1`, `probe-dash-2`) and neutral fixtures (`com.acme`) are fine. Scrub
before every commit — a leaked identifier in pushed history is a real exposure.
