# Restructure Proposal: Crate Topology

Date: 2026-05-05
Status: partially implemented; moved from `design/proposed/` on 2026-05-12; refreshed 2026-05-14
Related: `design/partial/restructure-ast.md`

Note: this plan is no longer a pure proposal. The repo now has a `[lib]`
target, `src/packets/`, `src/server/`, `src/tools/`, and first-pass child
splits under `src/tools/badgey/` and `src/workflow/engine/`. `src/main.rs` is
down to a daemon bootstrap/router shell, but it still owns many binary-local
module declarations and `src/lib.rs` is still a small shell. Keep this in
`design/partial/` until the crate topology work is closed or superseded.

## Original Problem

The crate originally had two god files and no library target:

| File | Lines | Role |
|------|-------|------|
| `src/main.rs` | 17,127 | Everything except `main()`: 109 named `#[tool]` handlers, Badgey wrapper glue, HTTP routes, progress notifications, app state, bootstrap, tests |
| `src/packets.rs` | 6,353 | Complete rule engine: AST, compiler, evaluator, fidelity auditor, self-heal scanner — plus a 3,443-line `#[cfg(test)]` block |

That is no longer the current tree: the `[lib]` target exists and `packets`,
`server`, and `tools` have been split. The remaining issue is not "no lib";
it is finishing ownership transfer from binary-local modules into the lib and
continuing to split large domain modules into smaller implementation files.

Routing-related concerns are now mostly split: `src/routing.rs`,
`src/webhooks.rs`, `src/pollers.rs`, `src/crons.rs`, `src/workflow/wait.rs`,
`src/server/routes.rs`, and `src/server/tail.rs` exist as separate files.
`src/mcp_tools/` contains the agentic graph helper set. The remaining
concentration has shifted away from `main.rs` and into a few large but more
domain-specific modules, especially `src/workflow/engine.rs`,
`src/workflow/ops.rs`, `src/tools/atoms.rs`, and `src/orchestration/providers.rs`.

## Current main.rs Breakdown

As of 2026-05-14, `src/main.rs` is 1,186 lines. It is no longer the 17K-line
god file described above. Its current shape is:

| Lines | Size | Content |
|-------|------|---------|
| 1–75 | 75 | Binary-local `mod` declarations, including many modules that should eventually be lib-owned |
| 76–127 | 52 | Imports and compatibility shim imports from `blackbox::...` |
| 129–169 | 41 | `BlackboxServer::new` router sum |
| 171–226 | 56 | Re-export/import glue plus now-empty legacy `bbox_tools` / `bro_tools` router impls |
| 228–1186 | 959 | Daemon bootstrap: config, logging, state open, router assembly, background tasks, graceful shutdown |

The highest-value remaining `main.rs` work is not more tool extraction; it is
finishing the module ownership move into `lib.rs`, moving `BlackboxServer::new`
and the bootstrap body into `server`, then deleting the empty legacy router impl
blocks once their router names are no longer needed.

## Current Packets Breakdown

`src/packets.rs` has already been converted to `src/packets/`. The current
layout is:

- `packets/ast.rs` — predicate AST types and parsing
- `packets/compile.rs` — rule compilation and normalization
- `packets/apply.rs` — first/all evaluation
- `packets/audit.rs` — fidelity auditing
- `packets/scanner.rs` — self-heal scanner and repair candidates
- `packets/coerce.rs` — JSON string-to-structure coercion
- `packets/events.rs` — packet event logging
- `packets/mod.rs` — store and public API
- `packets/tests.rs` — remaining shared tests

Remaining packet cleanup is test-locality work: move tests out of the shared
`packets/tests.rs` island into per-module `#[cfg(test)]` blocks when making
nearby packet changes.

## Proposed Structure

Already-split modules (kept as-is): `src/routing.rs`, `src/webhooks.rs`,
`src/pollers.rs`, `src/crons.rs`, `src/workflow/`, `src/orchestration/`,
`src/index/`, `src/chunker/`, `src/council/`, `src/providers/`,
`src/system_memory/`, `src/mcp_tools/`, `src/search/`, `src/vectors/`,
`src/embed/`.

Already landed additions: `src/lib.rs`, `src/server/`, `src/tools/`,
`src/packets/`, `src/tools/badgey/`, and `src/workflow/engine/`.

```
Cargo.toml                      # [lib] exists; all [[bin]] targets depend on it

src/
  lib.rs                        # Exists, but still a small shell. Continue
                                # migrating safe binary-local module
                                # declarations here in small batches.

  main.rs                       # Shrunk to ~1.2K lines. Remaining work:
                                # move router construction + daemon bootstrap
                                # into server, then keep only the binary entry.
  cli.rs                        # Unchanged ([[bin]] target for `bro`)
  irc_bridge.rs                 # Unchanged ([[bin]] target for `bro-irc`)
  slack_bridge.rs               # Unchanged ([[bin]] target for `bro-slack`)

  server/                       # Exists. Owns BlackboxServer + small
                                # subsystems. NOT a god-module — see §5
                                # for why the large current main.rs blocks
                                # splits across multiple targets.
    mod.rs                      # BlackboxServer struct + new() constructor
                                # that sums all tools/<domain>::*_tools()
                                # routers (~300-500 lines). Contains the
                                # tiny ServerHandler impl (7 lines, just
                                # `get_info`) — no need for its own file.
    state.rs                    # SharedState struct + impl + build_app_state()
                                # (extracted from main.rs lines 89-316)
    progress.rs                 # MCP progress-token plumbing for blocking
                                # waits — self-contained subsystem
                                # extracted from lines 4559-4831.
    badgey.rs                   # No longer the primary target for Badgey
                                # wrapper internals; see tools/badgey/.
    dispatch.rs                 # Free functions: dispatch_routed_event,
                                # validate_workflow_capabilities, related
                                # helpers. Referenced by sibling files
                                # (crons.rs, pollers.rs, workflow/engine.rs,
                                # council/*) via crate::* — re-exported
                                # from lib.rs for ergonomics.
    routes.rs                   # Axum HTTP route handlers (Bro roster
                                # endpoint, IRC/admin/orchestrate/webhook
                                # helpers).
    tail.rs                     # SSE /tail endpoint.

  tools/                        # Exists. One file per tool domain.
    mod.rs                      # `pub mod` declarations + shared param
                                # types if any
    bbox_search.rs              # bbox_search, bbox_cite, bbox_context,
                                # bbox_session, bbox_messages, bbox_topics,
                                # bbox_sessions_list, bbox_stats
    bbox_graph.rs               # bbox_hybrid_search,
                                # bbox_discover_seed_entities,
                                # bbox_inspect_entity, bbox_describe_schema,
                                # bbox_find_paths, bbox_bundle_evidence,
                                # bbox_blame
    bbox_projects.rs            # bbox_project_register,
                                # bbox_project_rename, bbox_project_list
    bbox_provenance.rs          # bbox_provenance_export/import
    bbox_embeddings.rs          # bbox_reembed, bbox_embed_status
    bbox_artifacts.rs           # bbox_artifact_install/list/supersede
    bbox_knowledge.rs           # bbox_learn, bbox_remember, bbox_decide,
                                # bbox_knowledge, bbox_review, bbox_render,
                                # bbox_knowledge_link, bbox_forget,
                                # bbox_lint
    bbox_threads.rs             # bbox_thread, bbox_thread_list
    bbox_notes.rs               # bbox_note, bbox_notes, bbox_note_resolve
    bbox_packets.rs             # bbox_compile, bbox_apply, bbox_audit,
                                # bbox_packet_list, bbox_packet_events,
                                # bbox_packet_gap
    bbox_inbox.rs               # bbox_inbox
    bbox_pins.rs                # bbox_pin
    bbox_bootstrap.rs           # bbox_bootstrap, bbox_absorb, bbox_reindex
    bro_exec.rs                 # bro_exec, bro_resume, bro_status, bro_wait,
                                # bro_cancel, bro_when_all, bro_when_any
    badgey.rs                   # Badgey public tool facade. Child modules:
    badgey/lifecycle.rs         # exec/resume/session lifecycle internals
    badgey/proposals.rs         # proposal/action apply/reject/dismiss internals
    badgey/reports.rs           # status/list/collect/triage/close-loop internals
    bro_team.rs                 # bro_team, bro_broadcast, bro_dashboard,
                                # bro_brofile
    bro_webhook.rs              # bro_webhook_install, bro_webhook_list,
                                # bro_webhook_replay, bro_webhook_deliveries
    bro_cron.rs                 # bro_cron_install, bro_cron_list,
                                # bro_cron_upcoming
    bro_poller.rs               # bro_poller_install, bro_poller_list
    bro_orchestrate.rs          # bro_orchestrate_run, bro_orchestrate_author,
                                # bro_workflow_install, bro_workflow_list
    bro_arc.rs                  # bro_arc_status, bro_arc_signal,
                                # bro_arc_cancel, bro_signals
    bro_agents.rs               # bro_agent_list/get/describe/search/dispatch
    bro_slack.rs                # bro_slack_bind
    whiteboard.rs               # whiteboard_open/register/post/state/
                                # annotate/vote/transition/conflicts/
                                # summarize/archive
    bro_council.rs              # bro_council_list, bro_council_open,
                                # bro_council_posts
    bro_mcp.rs                  # bro_mcp
    bro_providers.rs            # bro_providers
    bro_prune.rs                # bro_prune

  packets/                      # CONVERTED from src/packets.rs.
    mod.rs                      # Packets store + public API (~300 lines)
    ast.rs                      # Predicate AST types
    compile.rs                  # Rule compilation + normalization
    apply.rs                    # Evaluation engine
    audit.rs                    # Fidelity auditing
    scanner.rs                  # Self-heal scanner + repair candidates
    coerce.rs                   # JSON string→structure coercion
    events.rs                   # PacketEvent logging
    test_support.rs             # #[cfg(test)] shared fixtures

  routing.rs                    # Existing — keep as-is.
  webhooks.rs                   # Existing — keep as-is.
  pollers.rs                    # Existing — keep as-is.
  crons.rs                      # Existing — keep as-is.
  mcp_tools/                    # Existing — keep as-is (agentic graph helpers).

  workflow/                     # Existing directory — fine as-is.
    engine/fanout.rs            # Dynamic fanout runner plus fanout support
                                # structs/helpers.
  orchestration/                # Existing directory — fine as-is.
  index/                        # Existing directory — fine as-is.
  chunker/, council/, providers/, system_memory/  # Existing — fine.

  knowledge.rs                 # ~3K lines — split into knowledge/ if it grows.
  parser.rs                    # ~2K lines — self-contained, fine.
  threads.rs, notes.rs         # Fine.
  tool_docs.rs                 # ~1.3K lines — fine.
  render.rs, entity_ref.rs, util.rs, artifacts.rs, edge_index.rs,
  inbox.rs, pins.rs, projects.rs, query.rs, git.rs, mcp_client.rs,
  embed_queue.rs, whiteboards.rs, council_tui.rs,
  slack_thread_store.rs, slack_channel_bindings.rs # All existing siblings.
```

**Historical rename mechanics for `packets.rs` → `packets/`:**
This move has already landed. The important invariant remains: Rust does not
allow both `src/packets.rs` and `src/packets/mod.rs` to define the `packets`
module simultaneously. The original rename had to be a single atomic git move:

1. `git mv src/packets.rs src/packets/mod.rs` (creates the directory)
2. Inside the new `mod.rs`, add `pub mod ast; pub mod compile; ...` declarations for the sibling files.
3. Per sibling extraction, move code from `mod.rs` into `ast.rs` / `compile.rs` / etc., adding `use super::*;` as needed.

`cargo build` after every step verifies the move. Don't try to create the directory and the file simultaneously; pick one.

## Key Moves

### 1. Add a `[lib]` target

The single highest-impact change. `Cargo.toml` gains:

```toml
[lib]
name = "blackbox"
path = "src/lib.rs"

[[bin]]
name = "blackboxd"
path = "src/main.rs"

[[bin]]
name = "bro"
path = "src/cli.rs"

[[bin]]
name = "bro-irc"
path = "src/irc_bridge.rs"

[[bin]]
name = "bro-slack"
path = "src/slack_bridge.rs"
```

All four binaries depend on the lib. `src/lib.rs` declares the modules; the binaries `use blackbox::*` instead of declaring their own `mod foo;`:

```rust
// src/lib.rs
pub mod server;
pub mod tools;
pub mod packets;
pub mod routing;
pub mod orchestration;
pub mod index;
pub mod workflow;
pub mod knowledge;
pub mod parser;
pub mod threads;
pub mod notes;
pub mod tool_docs;
pub mod render;
pub mod entity_ref;
// ... etc
```

**Mechanical clarification:** modules declared with `mod foo;` in `main.rs` are owned by the binary crate; `lib.rs` cannot "re-export" them — it must `pub mod foo;` directly, taking ownership of the module. So the move is: every `mod foo;` line currently in `main.rs` is *deleted from main.rs* and *added to lib.rs*. The file `src/foo.rs` stays put on disk. The binary crate then sees `foo` only via `use blackbox::foo`, not via its own `mod foo;`.

This is a textual transformation, not a code move. The Rust compiler treats the file as belonging to whichever crate's `mod` declaration is active. Migrating the `mod` declarations from binary-owned to lib-owned is the actual content of step 1; the migration path's "move nothing" framing was misleading.

### 2. `main.rs` shrinks to ~100 lines

Only: initialize logging, call `server::run()` / `server::build_app_state()`,
bind the axum router, spawn the MCP listener, and handle graceful shutdown.
`main.rs` is now much closer to this goal, but the bootstrap body and router
construction still need to move into `server`.

```rust
use blackbox::server;

#[tokio::main]
async fn main() {
    tracing_subscriber::init();
    let state = server::build_app_state().await;
    let app = server::router(state.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 7264)).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### 3. One tool domain per file

The named `#[tool]` handlers cluster by prefix into domain files. Naming note:
file granularity is by *domain prefix*, not by single tool. `tools/badgey.rs`
now demonstrates the intended next-level pattern: keep public tool wrappers in
the domain facade and move large private implementation clusters into child
modules (`lifecycle`, `proposals`, `reports`).

**rmcp macro mechanics (load-bearing):** the `#[tool_router(router = NAME)]`
macro generates a router function from each annotated `impl` block. Multiple
`impl` blocks can each carry their own router, and `ToolRouter<Self>` instances
combine via `+`. The codebase uses this pattern heavily: the constructor sums
the small domain routers in `tools/*`, plus the legacy empty `bbox_tools` and
`bro_tools` routers until those compatibility stubs are deleted.

Splitting handlers across files therefore means: each `tools/<domain>.rs` declares its own `impl BlackboxServer` block annotated with `#[tool_router(router = <domain>_tools)]`, and the constructor sums them all:

```rust
// in server/mod.rs (or wherever BlackboxServer::new lives)
fn new(state: SharedState) -> Self {
    Self {
        state,
        tool_router:
              Self::bbox_search_tools()
            + Self::bbox_knowledge_tools()
            + Self::bbox_threads_tools()
            + Self::bbox_notes_tools()
            + Self::bbox_packets_tools()
            + Self::bbox_graph_tools()
            + Self::bbox_inbox_tools()
            + Self::bbox_pins_tools()
            + Self::bbox_bootstrap_tools()
            + Self::bbox_artifacts_tools()
            + Self::bro_exec_tools()
            + Self::badgey_tools()
            + Self::bro_team_tools()
            + Self::bro_agents_tools()
            + Self::whiteboard_tools()
            // ... etc
            ,
    }
}
```

Each `tools/<domain>.rs` file contains:
- Its parameter structs
- An `impl BlackboxServer` block with `#[tool_router(router = <domain>_tools)]` enclosing the `#[tool(...)] fn handler(...)` functions
- Any file-local helper functions
- Its own `#[cfg(test)] mod tests` block

`tools/mod.rs` declares the modules and re-exports if needed:

```rust
pub mod bbox_search;
pub mod bbox_knowledge;
// ...
pub mod bro_exec;
pub mod bro_team;
// ...
```

This adds visible boilerplate (one `impl` block per file declaring `#[tool_router(...)]`) but keeps the router-summing model the codebase already uses; no macro extension required.

### 4. `packets/` Directory

Each sub-module has a clear boundary:

- `ast.rs` — predicate types (`Predicate`, `FieldCmp`, `And`, `Or`, `Not`, `RankGe`, etc.) + parsing
- `compile.rs` — `compile_ruleset()`, normalization, validation
- `apply.rs` — `apply_first()`, `apply_all()`, entity evaluation
- `audit.rs` — `audit_dataset()`, fidelity reports, mismatch tracking
- `scanner.rs` — self-heal scanner, `RepairCandidate`
- `coerce.rs` — `coerce_stringified_params()`
- `events.rs` — `PacketEvent`, event logging

### 5. Large Modules → Split by Domain, NOT Wholesale

Routing is already split. The remaining concentration is now spread across
large domain modules rather than one `main.rs` god-file. Current examples:
`workflow/engine.rs` owns the core workflow runner, `workflow/ops.rs` owns hook
ops, `tools/atoms.rs` owns atom-facing tools, and `orchestration/providers.rs`
owns provider catalog/resolution. Split these by cohesive internal subsystem,
not by creating a second god-module.

Recent examples of the intended pattern:

- `tools/badgey.rs` keeps the public `#[tool]` wrappers while
  `tools/badgey/lifecycle.rs`, `tools/badgey/proposals.rs`, and
  `tools/badgey/reports.rs` own private implementation clusters.
- `workflow/engine/fanout.rs` owns dynamic fanout execution plus the support
  structs/helpers previously split between parent and child.

Wholesale extraction to a new catch-all module creates a new god-file and
conflicts with the per-domain `tools/*` extraction in §3.

**Correct split**: address subranges separately by what they actually are.

- **Tool handlers** → keep in the appropriate `tools/<domain>.rs` facade files. When a facade grows past wrapper code, split private implementation into child modules as done for Badgey.
- **Progress-token plumbing** → `src/server/progress.rs`. This is a self-contained subsystem (channels + correlation + token lifecycle).
- **Badgey wrapper internals** → now live under `src/tools/badgey/` because the implementation is tightly coupled to the public tool facade and `BlackboxServer` helper methods.
- **`dispatch_routed_event` + `validate_workflow_capabilities` + related free functions** → `src/server/dispatch.rs`. These are referenced from siblings via `crate::dispatch_routed_event`; once the lib reparenting (migration step 1) is done, those references resolve via `crate::server::dispatch::dispatch_routed_event` (re-exportable via `pub use` in lib.rs for ergonomics).
- **Workflow / parser helpers** → relocate to `workflow/` and `parser/` where they actually belong. Continue the `workflow/engine/fanout.rs` pattern for runner subsystems.
- **HTTP route handlers** → already live in `src/server/routes.rs`; split
  further into `server/admin.rs` / `server/orchestrate.rs` only if route-local
  changes justify it.

Split *during* the per-domain tools extraction (step 3 of the migration path), not as a separate phase. This avoids the trap of extracting a new god-file, then re-splitting it.

### 6. Tests relocate

The old 4,534-line `#[cfg(test)]` block at the bottom of `main.rs` has been
split out. Continue moving remaining broad test islands toward local tests:
- Unit tests → `#[cfg(test)] mod tests` inside their now-small domain files
- Integration tests → `tests/` directory, using `blackbox::` imports from the new lib

## Benefits

| Concern | Before | After |
|---------|--------|-------|
| Incremental review surface | 400-line diff in 17K-line file | 400-line diff in 400-line file |
| IDE navigation | `main.rs:5252` is meaningless | `tools/bbox_search.rs:47` is self-locating |
| `bro` / `bro-irc` / `bro-slack` reuse target | Each duplicates DTOs as ad-hoc serde structs | Shared `blackbox` crate types: tool param/response DTOs, `EntityRef`, `AgentSession`, Slack binding DTOs, etc. (Note: sidecars are HTTP/SSE clients today; direct daemon-state access stays out of reach since shared state is daemon-owned. The reuse target is shared types and small helpers, not direct call-through.) |
| Integration testing | Impossible without full binary | `tests/` directory with `use blackbox::*` reaches lib API |
| Parallel crate compilation | None (single binary crate) | lib + 4 bins can compile in parallel; bins link against the lib unit |
| New contributor orientation | Read 17K lines to find anything | File names match domain names |
| Compile-time impact | Single 17K compilation unit | Smaller compilation units, but rmcp's `#[tool_router]` and `#[tool_handler]` macro expansions still require the enclosing crate-unit's typecheck. The expected gain is *incremental locality* (rust-analyzer responsiveness, smaller blast radius for typo-fix rebuilds), not a per-file recompile model. |

## Migration Path

Each step is `cargo build && cargo test`-verified; no step is allowed to leave the tree non-compiling.

**Two structural constraints shape the migration order:**

1. **Rust orphan rule on inherent impls.** Once `BlackboxServer` lives in the lib crate, the binary crate cannot add `impl BlackboxServer { ... }` blocks. Tool impls therefore need to live in the same crate as `BlackboxServer`; the current tree keeps this true by keeping the tool modules and `BlackboxServer` ownership aligned.
2. **Sibling cross-references.** Files like `crons.rs`, `pollers.rs`, `workflow/engine.rs`, `council/*` reference symbols inline in `main.rs` — `crate::SharedState`, `crate::BlackboxServer`, `crate::dispatch_routed_event`, `crate::validate_workflow_capabilities`. Once a sibling becomes lib-owned (`pub mod foo;` in lib.rs), its `crate::*` references resolve against the LIB crate. Those symbols must therefore exist in the lib at that moment, not still-inline in the binary.

The combined constraint remains: **either reparent all shared symbols from
binary to lib in one mechanical step, or move sibling module declarations in
small batches only when their `crate::*` dependencies already resolve from the
lib.** The current tree is following the second path: incremental module
ownership moves plus domain-local decomposition.

Landed:

1. `[lib]`, `server/`, `tools/`, and `packets/` exist.
2. `SharedState`, progress-token plumbing, route handlers, tail SSE, dispatch
   helpers, and workflow runtime helpers are already under `server/`.
3. Tool domains are already split under `tools/`; `tools/badgey.rs` is now a
   facade with `lifecycle`, `proposals`, and `reports` child modules.
4. `workflow/engine/fanout.rs` owns fanout runner support.

Next useful cuts:

1. **Continue safe module ownership moves into `lib.rs`.**
   - Use the refactor runner's primitive sequence rather than ad hoc edits:
     `copy_rust_mod_decls` from `main.rs` to `lib.rs`,
     `delete_rust_items` in `main.rs`, `add_rust_use_decl` for the root alias,
     then `cargo check --bin blackboxd`.
   - Move modules in small batches where unqualified root aliases are enough
     (`use blackbox::<module>;`). Avoid large batches while sibling `crate::*`
     dependencies are still mixed between binary and lib ownership.

2. **Move daemon bootstrap out of `main.rs`.**
   - Move router construction and most of `main()` into `server`, leaving
     `main.rs` as a small `#[tokio::main]` entry point.
   - Delete the empty legacy `bbox_tools` / `bro_tools` router impls when the
     constructor no longer needs them.

3. **Keep splitting large domain modules by cohesive internals.**
   - `workflow/engine.rs`: continue with runner subsystems after fanout.
   - `workflow/ops.rs`: split hook/action families if they continue to grow.
   - `tools/atoms.rs`: keep the public tool facade and move implementation
     clusters into child modules.
   - `orchestration/providers.rs`: split catalog, credentials, and provider
     resolution when touching nearby code.

4. **Improve test locality.**
   - Move `packets/tests.rs` cases into per-module test blocks as those modules
     change.
   - Move daemon-startup-shaped tests to top-level integration tests using
     `blackbox::` imports.

Each cut should remain a small `cargo check` / targeted-test verified commit.
