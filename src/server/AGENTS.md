# src/server — daemon bootstrap, wire MCP head, surface evaluation

- The wire head extracts `?surface=` AND `?project=` once at `initialize`,
  resolves the project selector through the Read-intent resolver (alias /
  id / path → base canonical path, literal fallback for parity with
  bbox_mcp_surface), and pins both in per-session OnceLocks. Every
  get_tool / list_tools / call_tool surface lookup must pass the pinned
  pair — the SurfaceDecisionCache keys `(surface, project)`, and a lookup
  that hardcodes `None` for project silently reverts project-scoped surface
  packets to dispatch-path-only (gap-310c36b6 was exactly that).
- Surface evaluation runs BEFORE the session pins are set, and the deny
  verdict must abort initialize — a denied surface that still pins would
  leave a half-initialized session answering tool lists.
- Resolution at initialize does blocking fs/git probes → blocking pool, like
  every other resolver call site.
- Startup ordering in open.rs is load-bearing and documented inline: repo-
  owned stores (knowledge, gaps) load their committed files BEFORE any save
  can run, or the in-memory set purges the repo's files; alias
  materialization follows registry open and must tolerate per-repo failure
  (skip + warn — boot cannot fail closed the way registration does).
- `run` owns the startup order: load config ONCE, claim the instance-lock set,
  migrate legacy defaults, initialize file logging, then `open_shared_state`.
  Nothing that reads, repairs, moves, or creates durable state may move above
  the claim, and `open_shared_state` takes the loaded config plus the held
  `InstanceLockSet` rather than reloading (a reload could resolve roots the
  claim does not cover). `run` holds the set for the process lifetime.
- The legacy migration's DESTINATIONS come from that loaded config
  (`run::legacy_destinations`), never from a second env/`$HOME` derivation:
  recomputing them ignored the config file, so a config-file-isolated daemon
  moved shared legacy state into production-default paths it had not claimed.
  Its SOURCE (`~/.claude-shared`, `~/.bro`) belongs to no daemon, so
  `migrate_legacy_defaults` takes a non-blocking claim on
  `<home>/.blackbox-legacy-migration.lock` and SKIPS the migration when it
  loses; the winner did it or will.
- Every probe in that migration is fallible: only `NotFound` means absent, and
  any other inspection error refuses startup BEFORE a destination is created.
  `Path::exists()`/`is_dir()` collapse `EACCES`/`EIO` into `false`, which made
  a transient failure look like "already migrated" and permanently stranded
  the legacy source on the next boot.
- Each entry moves as a recoverable transaction journaled at
  `<home>/.blackbox-legacy-migration.journal`, beside the source claim (the
  one object every daemon shares). Files AND directory trees stage at
  `<dest>.migrating.tmp` with their contents and directories fsynced, publish
  by rename, record publication BEFORE the source is deleted, then fsync the
  source parent. `recover_legacy_migration` runs first under the claim and
  either rolls back an unpublished stage or finishes a published one, so a
  crash mid-move cannot leave a committed destination next to a stale source
  for a differently-rooted daemon to migrate again.
- Journal updates are atomic durable replacements, never in-place rewrites:
  a unique `O_EXCL` sibling under `.blackbox-legacy-migration.journal.*.tmp`
  is written, fsynced, renamed over the journal, then `$HOME` is fsynced. An
  in-place truncate-and-rewrite could leave the journal EMPTY, and an empty
  journal read as "nothing in flight" is precisely the duplicate-authority
  failure. A journal that exists but is empty, oversized, non-regular, or
  unparseable REFUSES startup with an operator-actionable message; only its
  absence means no transaction. Recovery sweeps stale staging siblings.
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
  relies on the per-store locks instead.
- `run_blocking`'s per-call log line (`tool`, `elapsed_ms`, `bytes`) is the
  only built-in tool telemetry; keep it intact when wrapping handlers.
- MCP response budgets cover the serialized result, including text escaping
  and structured content. Oversize is an explicit tool error, never an
  automatic filesystem export. Producers own pagination and detail reads;
  clients own any local persistence of received results. Domain outcomes such
  as a failed task remain distinct from invocation errors.
- Deferred EdgeIndex startup is a fail-closed warmup, never an empty graph.
  The watcher immediately publishes the first complete sidecar view and graph
  consumers return `error.edge_index_warming` until that publication lands.
  Selector-changing publications lower the readiness fence before publishing
  their intentionally empty placeholder, then nudge the same watcher; a graph
  reader may retain a complete old immutable view or wait for the complete new
  one, but may never observe the placeholder as a valid graph.
- Raw `?project=` remains a surface/filter selector only. Attended blame and
  provenance export use separate producer-token grants bound to a committed
  published scope; neither grant implies managed-workspace knowledge or
  mutation authority, and the three authority lanes are mutually exclusive.
