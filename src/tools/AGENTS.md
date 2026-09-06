# src/tools — MCP tool adapters

- Adapters stay thin; behavior lives in domain modules below. Every `#[tool]`
  needs a matching tool_docs.rs stanza (tests enforce) and blocking work goes
  through `run_blocking`/`spawn_blocking` — clippy's disallowed-methods gate
  denies blocking fs here, and sanctioned exceptions carry reasoned
  `#[allow]`s (scope.rs is the precedent). `scripts/lint-concurrency.sh` is
  the shape backstop when touching handlers.
- Project-scoped WRITE paths go through `resolve_project_write_scope`:
  durable key = registered base canonical path, write_dir = Some only for
  managed worktrees. Filter-side rescoping maps recognized selectors
  (worktree paths, ids, aliases) to the base and passes substring filters
  through untouched. The Read/Write gate asymmetry is owned by
  bbox-indexing's resolver — adapters consume it, never re-derive it. When
  adding a project-like param to a tool, wire `resolve_project_context`
  rather than inventing a chain (gap-de82a74d and the 2026-06 consolidation
  are what bespoke chains cost).
- The per-call `bytes` field logged at `blackbox::tool` is the radar for
  chronic oversized producers. Response helpers budget the complete result
  and return a typed error on overflow. Producers must provide bounded
  pagination, projections, and detail reads; the server does not spill tool
  responses into files or assume the client shares its filesystem.
- `bbox_knowledge`'s mixed response is deliberately progressive: durable
  knowledge gets the primary top-N, while rule-packet and system-memory matches
  are bounded sidecars with explicit expansion breadcrumbs. Do not "fix" recall
  spill by changing match/rank semantics unless relevance is actually wrong;
  producer-side shape belongs in limits, signposts, and exact/list follow-ups
  (gap-c1c1b304).
- Store-mutating tools observe multi-tenancy: the daemon serves several
  concurrent agents and worktrees; durable writes key to the BASE project so
  every checkout sees them, while repo-owned files land in the writer's
  checkout to travel with its branch.
