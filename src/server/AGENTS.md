# src/server: daemon bootstrap, wire MCP head, surfaces

- Request scope is the surface with its visible tool set, the project
  selector and the workspace binding grant. `resolve_request_scope` derives
  all of it from transport context (`?surface=`, `?project=`, the workspace
  binding header): the surface against the configured surface table
  (`config.surfaces`), the project through the Read-intent resolver (alias /
  id / path → base canonical path, literal fallback). `initialize` resolves
  once and pins the result in the session handler's OnceLocks, which
  get_tool / list_tools / call_tool read. A request reaching an unpinned
  handler is served by a fresh instance bound to that request's own scope
  (`scoped_for`); it never falls back to `default` when the request names
  something else, and the shared handler stays unpinned.
- Every MCP method resolves or checks request scope, including the ones
  whose answer does not vary by surface (resources, prompts, `skills/list`):
  a request naming an unknown surface or an unauthenticated binding is
  refused on all of them, never served a partial catalog.
- Catalog listings (tools, resources, prompts) are stable in order and carry
  `ttlMs` and `cacheScope: private` only for a request that reached an
  uninitialized handler on a revision without a handshake. A handler that
  initialized never adds them, whatever version a request's `_meta` claims.
- The wire head supports handshake protocol revisions only
  (`supported_protocol_versions`, `get_info`) and refuses `server/discover`.
  The SDK routes any supported no-handshake revision to a stateless path
  that never calls `initialize`. `get_tool` has no request context and
  resolves `default` on that path, and unpinned resolution pays the blocking
  project probe on every call. Widening the supported set is a deliberate
  gate, never a side effect of an SDK bump.
- Tool results are built through `BlackboxServer::tool_result`, which leaves
  the result-type discriminator absent: the same value is serialized on the
  legacy MCP wire, in `/control/*` replies and for the response budget.
- A refused scope (unknown surface, unauthenticated binding) must fail
  BEFORE any slot is set: resolution returns the whole scope or an error,
  and `pin_scope` sets every slot together, so no half-initialized handler
  answers tool lists.
- Scope resolution does blocking fs/git probes → blocking pool, like every
  other resolver call site.
- Startup ordering in open.rs is load-bearing and documented inline: repo-
  owned stores (knowledge, gaps) load their committed files BEFORE any save
  can run, or the in-memory set purges the repo's files; alias
  materialization follows registry open and must tolerate per-repo failure
  (skip + warn — boot cannot fail closed the way registration does).
- `run` owns the startup order: load config ONCE, claim the instance-lock set,
  initialize file logging, then `open_shared_state`.
  Nothing that reads, repairs, moves, or creates durable state may move above
  the claim, and `open_shared_state` takes the loaded config plus the held
  `InstanceLockSet` rather than reloading (a reload could resolve roots the
  claim does not cover). `run` holds the set for the process lifetime.
- The vector store is one config-resolved root (`paths.vectors_path`), not a
  derivation. The runtime store, the background embed lane, the migration
  inventory, the retirement discharge and reprobe, and history materialization
  all read that value, so an empty inventory means no rows rather than the
  wrong directory. `bbox_vectors::default_vectors_dir()` is only the default
  the config resolution falls back to, and `install_global_root` pins it
  before anything reaches `vectors::global()`.
- The claim covers EVERY mutable root the config resolves, not just
  `state_dir` (`instance_lock.rs::instance_lock_roots`): the transcript index
  defaults to the XDG data dir, and `BRO_HOME`, the packet/artifact dirs, and
  each JSON store carry independent overrides, so two daemons with distinct
  state roots otherwise share a Tantivy index. The vector root is claimed on
  the same footing since R33F1 made it config-resolved. Roots are canonicalized,
  deduplicated, and reduced by containment; refusal names the contended root
  and lists every claimed root. The state root keeps its lock inside itself
  (`<state_dir>/instance.lock`); every other root uses a sibling
  (`<root>.instance.lock`) because store directories reject foreign entries.
  The listener bind is NOT exclusivity for this purpose: it happens after the
  corpus index opens, after local-activation recovery, and after the
  coordinator-held pin clear, which unlinks writer temporaries a live peer
  daemon may still be publishing through. The offline `blackbox` CLI
  deliberately does not take these locks; it cannot reach those paths and
  relies on the per-store locks instead. An offline writer of a store the
  daemon holds in memory probes them with `held_instance_lock_covering`
  (existing lock files only, an instant shared lock, nothing kept):
  `producer-claims revoke` refuses while any instance lock covering the
  claims store is held, since its per-store lock is keyed to a projects path
  the CLI and the daemon can resolve differently.
- `run_blocking`'s per-call log line (`tool`, `elapsed_ms`, `bytes`) is the
  only built-in tool telemetry; keep it intact when wrapping handlers.
- MCP response budgets cover the serialized result, including text escaping
  and structured content. Oversize is an explicit tool error, never an
  automatic filesystem export. Producers own pagination and detail reads;
  clients own any local persistence of received results. Domain outcomes such
  as a failed task remain distinct from invocation errors.
- The daemon keeps no in-memory edge graph. The pinned code read view
  carries active selectors, the searcher, the catalog epoch and the Git
  overlay map. The code read view refresher republishes it when the manifest
  authority, the registered corpus project set or the catalog epoch changes,
  or when nudged (registration, unregistration and derived-manifest
  publishers nudge it); otherwise it refreshes only the searcher when the
  document count moved. It reads the manifest only and never parses edge
  rows. Code-source activations publish a complete view directly. The view's
  Git overlay selectors are the Git source GC roots, so maintenance protects
  exactly the sources they name.
- Git history has one writer per repository: producer transport owns it
  exactly while the repository's committed activation journal is current
  (`history_activation::transport_owns_project_history`). The reindex
  Git-history lease and the post-activation checkout walk both ask that
  rule; no marker or receipt takes part. Anything that can move a grant,
  membership or code selector without a code activation must reconcile
  journal currency (clear the stale overlays) or the local lane and the
  stale overlay will both claim the history.
- Before bind, a one-time pass removes the retired edge families and stamps
  a store-level marker; later starts cost one stat.
- Raw `?project=` remains a surface/filter selector only. Managed-workspace
  authority comes only from the workspace binding header, and it covers
  exactly the render locality exchange and project write routing. Knowledge,
  gap, and graph reads are published-only and never consult the binding.
- Startup removes retired provisional knowledge-source state (the
  `provisional/` store directory, `provisional-*` journals, and the operator
  bindings file) before the store opens. It is bounded to those members,
  idempotent, logged on failure, and never read.
