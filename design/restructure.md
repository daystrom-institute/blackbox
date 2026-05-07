# Restructure Proposal: Crate Topology

Date: 2026-05-05
Status: proposal, baseline refreshed 2026-05-07

## Problem

The crate has two god files and no library target:

| File | Lines | Role |
|------|-------|------|
| `src/main.rs` | 17,127 | Everything except `main()`: 109 named `#[tool]` handlers, Badgey wrapper glue, HTTP routes, progress notifications, app state, bootstrap, tests |
| `src/packets.rs` | 6,353 | Complete rule engine: AST, compiler, evaluator, fidelity auditor, self-heal scanner — plus a 3,443-line `#[cfg(test)]` block |

`Cargo.toml` has four `[[bin]]` targets (`blackboxd`, `bro`, `bro-irc`, `bro-slack`) and no `[lib]`. The sidecars cannot import shared types from the daemon (they use HTTP/SSE today; the gain from a `[lib]` is shared DTOs and small helpers, not direct daemon-state access — see Benefits). Integration tests are impossible without running the full binary. IDE navigation is hostile (`main.rs:5252` is meaningless).

Routing-related concerns are already partially split: `src/routing.rs`, `src/webhooks.rs`, `src/pollers.rs`, `src/crons.rs`, and `src/workflow/wait.rs` exist as separate files. `src/mcp_tools/` already contains the agentic graph helper set (hybrid search, seed discovery, inspect, find paths, bundle evidence, blame, provenance, describe schema). The remaining concentration is in `main.rs`: a huge `BlackboxServer` god-impl, Badgey wrapper state machine glue, tool handlers, web route handlers, bootstrap, and a 4.5K-line test module.

## Current main.rs Breakdown

The file uses `// ---` section dividers. Verified ranges from the actual file:

| Lines | Size | Content |
|-------|------|---------|
| 1–87 | 87 | `mod` declarations + imports |
| 89–316 | 228 | Shared state (`SharedState`, `ArcSnapshot`, signal/webhook event structs) |
| 318–3184 | **2,867** | `BlackboxServer` struct + helper methods + Badgey wrapper internals / adapter / restore-recovery helpers |
| 3186–4051 | 866 | `bbox_*` tools — `#[tool_router(router = bbox_tools)]` impl block |
| 4053–4557 | 505 | `bro_*` / Badgey / workflow / whiteboard parameter structs and support DTOs |
| 4559–4831 | 273 | Progress notifications — MCP `progressToken` plumbing for blocking waits |
| 4833–8637 | **3,805** | `bro_*`, `badgey_*`, `whiteboard_*`, `bro_agent_*`, `bro_council_*`, workflow/webhook/cron/poller/arc tools — `#[tool_router(router = bro_tools)]` impl block |
| 8639–9449 | 811 | Helper methods on `BlackboxServer` |
| 9451–9461 | 11 | `ServerHandler` impl — trivial; just `get_info` returning capabilities |
| 9463–11741 | **2,279** | HTTP route code: Bro roster, IRC/admin/orchestrate/webhook route helpers, signal dispatch helpers |
| 11743–11903 | 161 | Tail SSE endpoint |
| 11905–12592 | 688 | `main()` / daemon bootstrap |
| 12594–17127 | **4,534** | `#[cfg(test)] mod tests` |

The largest concentrations now are the **3,805-line bro-side tool block**, the **2,867-line `BlackboxServer` / Badgey helper block**, the **2,279-line HTTP route block**, and the **4,534-line test module**. Progress-token plumbing is small enough to extract cleanly, but it is no longer the dominant middle block. Treat the HTTP block as web-route extraction (→ `server/routes.rs` / `server/admin.rs` / `server/orchestrate.rs` as needed), not handler-router extraction.

## Current packets.rs Breakdown

A flat 6,353-line file. Lines 1–2,910 are non-test code; lines 2,911–6,353 (3,443 lines) are a single `#[cfg(test)] mod tests` block. Non-test breakdown (approximate):

- MCP parameter structs (~200 lines)
- Predicate AST types and parsing (~700 lines)
- Rule compilation and normalization (~400 lines)
- Evaluation engine — first/all modes (~500 lines)
- Fidelity auditing (~200 lines)
- Packet store (CRUD, listing, events) (~600 lines)
- Self-heal scanner and repair candidates (~300 lines)
- JSON string→structure coercion (~150 lines)
- Packet event logging (~200 lines)

**Internal dependencies (matters for split order):** `ast.rs` is a leaf; `compile.rs` consumes `ast.rs`; `apply.rs` consumes both `ast.rs` and `compile.rs`; `audit.rs` consumes `apply.rs`; the store + events + scanner consume the evaluator. Split bottom-up: `ast.rs` → `compile.rs` → `apply.rs` → `audit.rs` → `scanner.rs` → store/events. `coerce.rs` is independent and can move first.

**Test relocation:** the 3,443-line test block contains tests for every layer (AST parsing, compile, apply, audit, scanner). It cannot be wholesale-moved to one location. Per layer, extract its tests into a `#[cfg(test)] mod tests` inside the new sub-module. Shared fixtures (test rule sets, sample entities, helpers) factor into `packets/test_support.rs` (gated `#[cfg(test)]`) and the per-module test blocks `use super::test_support::*`.

## Proposed Structure

Already-split modules (kept as-is): `src/routing.rs`, `src/webhooks.rs`, `src/pollers.rs`, `src/crons.rs`, `src/workflow/`, `src/orchestration/`, `src/index/`, `src/chunker/`, `src/council/`, `src/providers/`, `src/system_memory/`, `src/mcp_tools/`, `src/search/`, `src/vectors/`, `src/embed/`.

Net additions: `src/lib.rs`, `src/server/`, `src/tools/`, and converting `src/packets.rs` → `src/packets/` directory.

```
Cargo.toml                      # Add [lib]; all [[bin]] targets depend on it

src/
  lib.rs                        # NEW. Declares modules + minimal re-exports
                                # for the binaries to consume.

  main.rs                       # SHRUNK. ~100 lines: parse args, call
                                # blackbox::server::build_app_state(),
                                # bind axum, run.
  cli.rs                        # Unchanged ([[bin]] target for `bro`)
  irc_bridge.rs                 # Unchanged ([[bin]] target for `bro-irc`)
  slack_bridge.rs               # Unchanged ([[bin]] target for `bro-slack`)

  server/                       # NEW. Owns BlackboxServer + small
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
    badgey.rs                   # Badgey wrapper internals that are not
                                # the public badgey_* tool handlers:
                                # scope bind, post-process actions,
                                # proposal apply/reject/dismiss, restore.
    dispatch.rs                 # Free functions: dispatch_routed_event,
                                # validate_workflow_capabilities, related
                                # helpers. Referenced by sibling files
                                # (crons.rs, pollers.rs, workflow/engine.rs,
                                # council/*) via crate::* — re-exported
                                # from lib.rs for ergonomics.
    routes.rs                   # Axum HTTP route handlers (Bro roster
                                # endpoint, IRC/admin/orchestrate/webhook
                                # helpers; extracted from lines 9463-11741).
    tail.rs                     # SSE /tail endpoint (lines 11743-11903).
                                # Optional — fold into routes.rs if small.

  tools/                        # NEW. One file per tool domain.
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
    badgey.rs                   # badgey_exec, badgey_resume, badgey_ask,
                                # badgey_dismiss, badgey_status, badgey_list,
                                # badgey_scout, badgey_collect,
                                # badgey_triage_inbox, badgey_close_loops
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

**Rename mechanics for `packets.rs` → `packets/`:**
Rust does not allow both `src/packets.rs` and `src/packets/mod.rs` to define the `packets` module simultaneously. The rename is a single atomic git move:

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

Only: initialize logging, call `server::run()` / `server::build_app_state()`, bind the axum router, spawn the MCP listener, and handle graceful shutdown. The current 17K-line file is mostly everything *except* main.

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

The 109 named `#[tool]` handlers cluster by prefix into domain files. (Naming note: file granularity is by *domain prefix*, not by single tool — e.g., `tools/bro_exec.rs` covers `bro_exec`/`bro_resume`/`bro_status`/`bro_wait`/`bro_cancel`/`bro_when_all`/`bro_when_any`; `tools/badgey.rs` covers the Badgey consultant wrapper surface.)

**rmcp macro mechanics (load-bearing):** the `#[tool_router(router = NAME)]` macro generates a router function from each annotated `impl` block. Multiple `impl` blocks can each carry their own router, and `ToolRouter<Self>` instances combine via `+`. The codebase already does this — `main.rs` has `#[tool_router(router = bbox_tools)]` at line 3216 and `#[tool_router(router = bro_tools)]` at line 4833, combined as `Self::bbox_tools() + Self::bro_tools()` in the constructor.

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

### 4. `packets.rs` (6.3K) → `packets/` directory

Each sub-module has a clear boundary:

- `ast.rs` — predicate types (`Predicate`, `FieldCmp`, `And`, `Or`, `Not`, `RankGe`, etc.) + parsing
- `compile.rs` — `compile_ruleset()`, normalization, validation
- `apply.rs` — `apply_first()`, `apply_all()`, entity evaluation
- `audit.rs` — `audit_dataset()`, fidelity reports, mismatch tracking
- `scanner.rs` — self-heal scanner, `RepairCandidate`
- `coerce.rs` — `coerce_stringified_params()`
- `events.rs` — `PacketEvent`, event logging

### 5. Large `main.rs` blocks → split by domain, NOT wholesale

Routing is **already partly split**: `src/routing.rs`, `src/webhooks.rs`, `src/pollers.rs`, `src/crons.rs`, and `src/workflow/wait.rs` exist. The remaining concentration in `main.rs` is spread across the `BlackboxServer` helper block, the two rmcp tool-router impl blocks, HTTP route helpers, bootstrap, and tests.

The old "progress notifications" label no longer describes the real concentration. Today the large blocks are:

- `BlackboxServer` helpers + Badgey internals (lines 318–3184)
- `bbox_tools` router impl (lines 3216–4051)
- `bro_tools` router impl (lines 4833–8637)
- HTTP/admin/orchestrate/webhook route helpers (lines 9463–11741)
- bottom test module (lines 12594–17127)

They are not one coherent dispatch concern. They include:
- MCP progress-token plumbing for blocking waits
- bro and Badgey tool handlers
- whiteboard, council, agent-manifest, Slack binding, workflow, webhook, poller, cron, and arc-signal tool handlers
- workflow validation params + helpers
- parser / workflow authoring helpers
- `dispatch_routed_event` and `validate_workflow_capabilities` (load-bearing free functions referenced from siblings: `crons.rs`, `pollers.rs`, `workflow/engine.rs`, `council/*` all `use crate::dispatch_routed_event`).

Wholesale extraction to one `server/dispatch.rs` would create a new god-file and conflict with the per-domain `tools/*` extraction in §3.

**Correct split**: address subranges separately by what they actually are.

- **Tool handlers** → move to the appropriate `tools/<domain>.rs` files in step 3 of the migration path. Split `bbox_*`, `bro_*`, `badgey_*`, `whiteboard_*`, and `bro_agent_*` surfaces into separate `#[tool_router]` impls.
- **Progress-token plumbing** → `src/server/progress.rs`. This is a self-contained subsystem (channels + correlation + token lifecycle).
- **Badgey wrapper internals** → `src/server/badgey.rs` (or `src/orchestration/badgey/wrapper.rs` if the dependency direction is cleaned up first). Keep public `badgey_*` handlers in `tools/badgey.rs`.
- **`dispatch_routed_event` + `validate_workflow_capabilities` + related free functions** → `src/server/dispatch.rs`. These are referenced from siblings via `crate::dispatch_routed_event`; once the lib reparenting (migration step 1) is done, those references resolve via `crate::server::dispatch::dispatch_routed_event` (re-exportable via `pub use` in lib.rs for ergonomics).
- **Workflow / parser helpers** → relocate to `workflow/` and `parser/` (or alongside `parser.rs` as a module split) where they actually belong.
- **HTTP route handlers** at 9463–11741 → `src/server/routes.rs` / `src/server/admin.rs` / `src/server/orchestrate.rs` depending on the extracted size.

Split *during* the per-domain tools extraction (step 3 of the migration path), not as a separate phase. This avoids the trap of extracting a new god-file, then re-splitting it.

### 6. Tests relocate

The 4,534-line `#[cfg(test)]` block at the bottom of `main.rs` splits:
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

1. **Rust orphan rule on inherent impls.** Once `BlackboxServer` lives in the lib crate, the binary crate cannot add `impl BlackboxServer { ... }` blocks. The 109-tool concentration on `BlackboxServer` therefore must move to the lib crate at the same time as the type itself, OR all `#[tool_router(...)]` impl blocks must be relocated to lib-owned files first while `BlackboxServer` is still in main.rs (the impl-then-type order).
2. **Sibling cross-references.** Files like `crons.rs`, `pollers.rs`, `workflow/engine.rs`, `council/*` reference symbols inline in `main.rs` — `crate::SharedState`, `crate::BlackboxServer`, `crate::dispatch_routed_event`, `crate::validate_workflow_capabilities`. Once a sibling becomes lib-owned (`pub mod foo;` in lib.rs), its `crate::*` references resolve against the LIB crate. Those symbols must therefore exist in the lib at that moment, not still-inline in the binary.

The combined constraint: **either reparent everything from binary to lib in one mechanical step, then split within the lib, OR leave shared symbols (SharedState, BlackboxServer, dispatch_routed_event, etc.) inline in main.rs and don't move sibling `mod` declarations until they're extracted.**

This skeleton picks the first path — one upfront reparenting, then incremental splits within the lib. Each "step" below is a single `cargo test`-verified commit.

1. **Reparent the crate from binary-owned to lib-owned (the one big-but-mechanical move).**
   - Add the `[lib]` stanza to `Cargo.toml`.
   - Create `src/lib.rs`. Transfer EVERY `mod foo;` declaration from `main.rs` to `lib.rs`, declared as `pub mod foo;`.
   - Move ALL inline content from `main.rs` into the lib: `SharedState` struct, `BlackboxServer` struct + every `impl BlackboxServer { ... }` block (the two big `#[tool_router]` blocks at 3216/4833, the helper blocks, the Badgey wrapper internals, the ServerHandler impl at 9455, the HTTP route handlers at 9463), the progress-token plumbing, the dispatch free functions (`dispatch_routed_event`, `validate_workflow_capabilities`), the Tail SSE endpoint. Drop them all into a single new `src/server/mod.rs` for now (a temporary god-module within the lib — to be split in subsequent steps).
   - `main.rs` shrinks to: `use blackbox::server; #[tokio::main] async fn main() { server::run().await }` plus a `pub async fn run()` in `lib::server` that does the bootstrap (existing main body).
   - **Sibling `crate::SharedState` etc. references now resolve correctly** because the symbols are in the lib crate.
   - `cargo build && cargo test`. This is one large commit because it's an atomic ownership transfer; subsequent steps are small and incremental.

2. **Extract `SharedState` into `src/server/state.rs`.**
   - Move the `SharedState` struct + impl block out of the now-large `server/mod.rs` into a sibling file.
   - Add `pub mod state;` in `server/mod.rs`. Add `pub use state::SharedState;` for ergonomic re-export.
   - `cargo test`.

3. **Extract progress-token plumbing into `src/server/progress.rs`.**
   - Self-contained subsystem; pulls cleanly out of the current 4559–4831 block.
   - Add `pub mod progress;` in `server/mod.rs`.
   - `cargo test`.

4. **Extract one tool domain at a time** into `src/tools/<domain>.rs`. Start with the most self-contained (e.g., `bbox_search`). For each extraction:
   - Move the relevant handlers + their parameter structs out of `server/mod.rs` into `tools/<domain>.rs`.
   - Wrap them in their own `impl BlackboxServer { ... }` block annotated with `#[tool_router(router = <domain>_tools)]`.
   - Update `BlackboxServer::new` (in `server/mod.rs`) to add `+ Self::<domain>_tools()` to the router sum.
   - `cargo test`.
   - The orphan rule is fine here: `BlackboxServer` and the impl block both live in the lib crate, just in different modules of the same crate.
   - When all 109 handlers are moved, the two original `#[tool_router]` blocks in `server/mod.rs` (relocated from main.rs in step 1) are empty and can be deleted.

5. **Extract HTTP route handlers (lines 9463–11741 from the original main.rs, now in `server/mod.rs`) into `src/server/routes.rs` / `src/server/admin.rs` / `src/server/orchestrate.rs`.**
   - Bro roster endpoint, IRC bridge endpoints, admin endpoints, orchestrate endpoints, webhook/replay endpoints.
   - `cargo test`.

6. **Extract Tail SSE into `src/server/tail.rs` (or fold into `routes.rs`).**

7. **Extract dispatch free functions into `src/server/dispatch.rs`.**
   - `dispatch_routed_event`, `validate_workflow_capabilities`, related helpers.
   - Re-export from `lib.rs` (`pub use server::dispatch::dispatch_routed_event;`) so sibling files can keep using `crate::dispatch_routed_event` without import churn — or update siblings to use `crate::server::dispatch::dispatch_routed_event`. Either is fine; pick one.
   - `cargo test`.

8. **Convert `src/packets.rs` → `src/packets/`.**
   - `git mv src/packets.rs src/packets/mod.rs`.
   - Inside `mod.rs`, declare `pub mod ast; pub mod compile; pub mod apply; pub mod audit; pub mod scanner; pub mod coerce; pub mod events; pub mod test_support;`.
   - Extract per-layer in dependency order: `ast.rs` first (leaf), then `compile.rs`, `apply.rs`, `audit.rs`, `scanner.rs`, store/events. `coerce.rs` is independent — move first or last, doesn't matter.
   - After each layer is extracted, move its tests from the original `mod tests` block into `#[cfg(test)] mod tests` inside the new sub-module file. Shared test fixtures factor into `test_support.rs`.

9. **Move tests** (concurrent with the above): after each extraction, the relevant `#[test]` functions live next to the code they test. Integration-shaped tests that need full daemon startup move to a top-level `tests/` directory and `use blackbox::*;`.

10. **Clean up `main.rs` to ~100 lines.** Anything still inline by this point is either truly main-only (CLI parsing, server bootstrap) or hasn't been extracted yet — finish the moves.

Step 1 is the only large commit. Steps 2–10 are each a small `cargo test`-verified move; if any step breaks, revert it and try a smaller cut.
