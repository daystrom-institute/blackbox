# AGENTS_BETA.md

This project doc is for agents running inside the bro-harness sandbox/beta path.
It is selected with `BRO_HARNESS_PROJECT_DOC_FILES=AGENTS_BETA.md`.

## Agentic Grounding Sequence

Treat grounding as task-critical context that is already present in the session.
The harness injects project docs and blackbox tool guidance before the task, so
do not spend the opening move re-reading files solely to prove they exist. Use
the injected `[project-docs]`, `[scope]`, cwd, project_dir, provider/session id,
MCP server name, and any worktree metadata as the initial source of truth. Open
source files only for exact lines, freshness checks, missing injected content,
or contradictions from live tool results. Report missing, ambiguous, stale, or
contradictory injected context as grounding friction.

Use this sequence before codebase, design, history, or coordination claims:

1. **Context acceptance** — identify what the injected context already
   established: selected project docs, standing rules, task scope, and any cited
   thread/design anchors. Do not restate or re-open injected instructions unless
   the next step needs exact lines, the injected copy appears stale, live tools
   disagree with it, or the file was not injected.
2. **Sandbox boundary** — run the normalized sandbox sequence:
   - For read-only orientation, call `sandbox_grounding(enter_worktree=false)`.
   - For any task that may edit files, call
     `sandbox_grounding(enter_worktree=true, purpose=<short reason>)` before
     editing. Treat the returned `worktree.cwd` as authoritative.
3. **Blackbox evidence bundle** — when the task depends on prior decisions,
   design docs, threads, code graph facts, or conversation history, use the
   blackbox opening sequence instead of memory:
   `bbox_describe_schema` once per session, `bbox_hybrid_search` for seeds,
   `bbox_inspect_entity` to confirm refs, `bbox_find_paths` only for multi-hop
   claims, and `bbox_bundle_evidence` before making provenance-sensitive
   claims. Pass returned entity refs/path IDs directly; do not reconstruct them
   from memory. Use `property_mode="summary"` when bundling broad tool/knowledge
   refs or other long entities. The detailed question-shape runbook is
   `sm-agentic-opening-sequence`; pull it only when the injected tool guidance is
   insufficient for the question shape. For fresh probe/retro evidence, if
   hybrid search returns only generic seeds, no results, or a degraded
   BM25-only/vector-warming notice, pivot to `bbox_notes`/`bbox_gaps` with exact
   task, project, bro, or short substrings before broadening to git/filesystem
   evidence. If `bbox_describe_schema` reports `project_file` /
   `project_file_v2` population `0`, do not investigate or patch indexing as
   part of a sandbox probe. State the corpus gap, dedupe/file a
   `sandbox-observability` gap if one does not already exist, and use
   `work_smart_read` or scoped file reads for exact code locations while still
   bundling any non-code bbox evidence that resolved cleanly.
4. **Work execution** — read/edit/validate in the grounded cwd/worktree, keeping
   evidence refs and validation output narrow enough for another agent to audit.

## Sandbox Boundary

The normalized sequence returns the launch sandbox manifest and, when requested,
the managed worktree plus a second manifest inspected at that worktree root. It
shows selected project docs, redacted task-local session env, shell-visible env,
git/worktree identity, and the cwd/root boundary.

If `sandbox_grounding` is unavailable, use the manual fallback: call
`sandbox_status`, call `enter_worktree` before edits, then call
`sandbox_status(root=<returned cwd>)`.

When `enter_worktree` is available and the task may edit files, use it before
editing. After entering, keep edits inside the returned worktree unless the
operator explicitly redirects you. Pass the worktree path to project-scoped bbox
calls so committed artifacts land with the branch. The worktree is created from
a committed git ref, so uncommitted files from the parent checkout may be present
in injected context but absent on disk inside the worktree. Report that as a
context/filesystem divergence instead of editing the parent checkout.

## Sandbox-Native Tooling

Prefer sandbox-scoped tools and idioms over host/outside-daemon assumptions:

- Prefer workspace/file tools for reads and edits instead of raw host paths.
- Prefer `work_bash` or the harness shell tool over unscoped shell assumptions.
- Prefer `sandbox_grounding` over reconstructing sandbox state from scattered
  shell probes; prefer `sandbox_status` for follow-up spot checks.
- After `enter_worktree`, prefer `work_*` tools or absolute paths under the
  returned worktree; generic file tools may still target the original checkout.
- Prefer direct in-sandbox knowledge/search/note affordances when present. If
  only MCP names are available, use the MCP tool and note the alias gap only when
  the extra ceremony materially affects the work.
- Keep tool outputs scoped and diagnostic. If a command or tool result is too
  large, narrow the query instead of flooding context.

## Authorial Surface

When the task is to author behavior, choose the lowest surface that matches the
durability and control-plane shape:

- **NARF cells** — use `narf_exec` for one-shot JS composition, or
  `narf_prepare` followed by `narf_run` when the rendered source/contract needs
  review before execution. A cell receives values, not ref envelopes. Host tools
  return values into the cell; use JS for transforms and return a compact
  summary, structured value, or KV name rather than a blob.
- **NARF dialect** — cells have `narf.encode.yaml`,
  `narf.encode.frontmatter`, and `narf.encode.mdTable` for non-JS-native output
  formats. Use `narf.kv.set/get/peek/delete` only on exact names the author
  already holds; in-box KV enumeration/search is intentionally absent. Use
  model-facing KV list/peek/get tools, when present, to survey keys before
  authoring a cell that dereferences them. Ordinary JS `await` is live within
  the current activation; cross-turn or restart-safe waiting requires an
  explicit durable handle from a host producer.
- **Refactor work** — pull `sm-refactor` and the language memory when the
  generic tool docs are not enough. Use `bbox_refactor_status` to inventory
  exact items/kinds before `bbox_refactor_plan`; apply only after reviewing a
  plan with `bbox_refactor_apply(confirm=true)`. LSP-backed kinds such as
  `rust_lsp_rename` should return a plan or fail closed with a clear LSP
  error/timeout. If an LSP-backed plan remains `tool_running` after a wait
  timeout, inspect `bro_status`, cancel only your own task if needed, and file a
  refactor gap with the tool call and idle timing.
- **Ad-hoc bro dispatch** — use `bro_exec` for a fresh child task,
  `bro_wait`/`bro_when_all` for joins, `bro_status(tail=N)` after timeouts or
  empty/suspicious completions, and `bro_resume` for continuity in the same
  session. Record `taskId` and the concrete `sessionId` once it resolves.
  Nested dispatch from a sandbox probe should be explicitly authorized, bounded
  to the requested fanout, and should instruct children not to recurse.

## Observability Contract

For sandbox probes, make the boundary observable as part of the work:

- Record what cwd/worktree/base repo you actually used.
- Record which file reads/writes and shell commands mattered.
- Call out whether denials, missing tools, env overrides, and MCP surface shape
  were visible enough to debug after the fact.
- In the retrospective, distinguish task difficulty from sandbox friction.

Use durable project knowledge only for settled invariants. Use notes/gaps for
probe findings, missing surfaces, and retrospective feedback.
