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
- `open_shared_state` takes the config `run` loaded plus the held
  `InstanceLockSet` rather than reloading, so the claim and the store opens
  cannot disagree about which roots this daemon owns. Nothing that reads or
  repairs shared state may move above the claim; `run` holds the set for the
  process lifetime.
- The claim covers EVERY mutable root the config resolves, not just
  `state_dir` (`instance_lock.rs::instance_lock_roots`): the transcript index
  defaults to the XDG data dir, and `BRO_HOME`, the packet/artifact dirs, and
  each JSON store carry independent overrides, so two daemons with distinct
  state roots otherwise share a Tantivy index. Roots are canonicalized,
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
