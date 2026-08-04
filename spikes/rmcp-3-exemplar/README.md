# rmcp 3.1 MCP 2026-07-28 Exemplar

This standalone crate is a reference implementation for the MCP mechanics in
`design/surfaces/mcp/mcp-2026-07-28-target-surface.md` and
`design/surfaces/mcp/rmcp-3-migration-plan.md`. It is intentionally detached
from the parent Cargo workspace.

Run it from this directory:

```bash
cargo test
cargo run --bin demo_client
```

The server binds an ephemeral loopback port. `/mcp` is the working
compatibility endpoint using `LocalSessionManager` with
`with_legacy_session_mode(true)`. `/stateless` uses `NeverSessionManager` with
the same legacy-mode setting to expose the exact requested configuration:
modern requests work statelessly, while a legacy initialize attempt fails at
session creation.

## 1. Stateless HTTP And Discovery

- Design contract: target surface, "Stateless wire head and per-request
  context" and "server/discover"; migration plan, Phase 1 items 3, 5, and 7.
- Spike symbol: `DemoServer::spawn`, `DemoServer::supported_protocol_versions`,
  `DemoClientHandler::{auto_mode,discover_mode}`.
- Production preview: `src/server/mcp.rs`, `src/server/handler.rs`,
  `src/mcp_client.rs`, and `crates/bro-harness/src/mcp.rs`.

rmcp 3.1 does not permit one service to combine `NeverSessionManager` with
working legacy sessions. `NeverSessionManager::create_session` always fails,
while legacy mode routes `initialize` through that method. The spike therefore
shows the valid configurations explicitly: a dual-stack compatibility endpoint
with `LocalSessionManager`, and a pure stateless endpoint with
`NeverSessionManager`.

## 2. Per-Request Scope Extraction

- Design contract: target surface, "Stateless wire head and per-request
  context"; migration plan, Phase 1 item 1 and Phase 4 resource filtering.
- Spike symbol: `DemoServer::scope`, called by `discover`, `list_tools`,
  `call_tool`, `list_resources`, and `read_resource`.
- Production preview: `src/server/handler.rs`, `src/server/state.rs`, and the
  `SurfaceDecisionCache`.

`surface` and `project` are parsed from the `http::request::Parts` extension on
every method. The `restricted` surface hides `demo_secret` and
`demo://brofile/restricted`.

## 3. Tasks Extension

- Design contract: target surface, "Tasks projection (SEP-2663)"; migration
  plan, Phase 2.
- Spike symbol: `DemoServer::create_task`, `DemoServer::{get_task,cancel_task}`,
  `update_task`, and the `demo_dispatch` branch in `call_tool`.
- Production preview: orchestration `TaskStore`, `bro_exec`, `bro_status`,
  `bro_cancel`, and `bro_report`.

`demo_dispatch` returns a pure `CreateTaskResult` only when the request declares
`io.modelcontextprotocol/tasks`. Other clients receive plain structured JSON.
The in-memory task store exposes TTL, polling hints, status messages, terminal
results, and cooperative cancellation through a `CancellationToken`.

## 4. subscriptions/listen

- Design contract: target surface, "Wake-on-done: three tiers"; migration plan,
  Phase 3.
- Spike symbol: `DemoServer::{accepted_subscription_filter,listen}`,
  `DemoEvent`, and `demo_mutate_surface`.
- Production preview: `ServerHandler::listen`, the orchestration task `Notify`
  path, and surface-packet mutation handlers.

`demo_mutate_surface` emits `notifications/tools/list_changed` through
`SubscriptionSink`. Task transitions are custom glue because rmcp 3.1 rejects
`notifications/tasks` in both `SubscriptionSink` and the client
`Subscription`. The spike sends those notifications on the active listen
response stream without subscription metadata, where `ClientHandler` receives
them. This is demonstrative SDK friction, not the final conformance shape.

## 5. MRTR And Request-State Integrity

- Design contract: target surface, "MRTR approval gates (SEP-2322)"; migration
  plan, Phase 5.
- Spike symbol: `DemoServer::call_deploy`, `DeployRoundState`, and
  `DemoClientHandler::create_elicitation`.
- Production preview: operator-confirmation flows such as consultant applies,
  destructive administration, and RX-V1 authority flags.

`demo_deploy` returns `InputRequiredResult` with a form elicitation. Its
`requestState` is sealed and verified with rmcp's HMAC
`RequestStateCodec`. Tests drive both automatic client rounds and manual
`call_tool_once` rounds, including tamper rejection.

## 6. Resource Plane

- Design contract: target surface, "Resource projection"; migration plan,
  Phase 4.
- Spike symbol: `DemoServer::{resource_catalog,list_resource_templates,
  list_resources,read_resource}`.
- Production preview: future `blackbox://` projections for brofiles, artifacts,
  atoms, packets, tasks, system memories, and spills.

The `demo://` plane provides brofile and task URIs, URI templates, descriptor
listing, reads, and cursor pagination with a page size of two. List and read
results carry private cache hints.

## 7. Cache Hints And Stable Tools

- Design contract: target surface, "Cache hints and tools/list hygiene
  (SEP-2549)"; migration plan, Phase 1 item 6.
- Spike symbol: `DemoServer::tools_for`, `LIST_TTL_MS`, and list/read handlers.
- Production preview: `tool_router.list_all()` ordering and MCP result adapters
  in `src/server/handler.rs`.

Tool definitions are sorted by name before every response. Tests assert stable
ordering plus `ttlMs` and `cacheScope: private`.

## 8. Progress Notifications

- Design contract: target surface, "Wake-on-done: three tiers", Tier 0;
  migration plan coupling inventory for progress notifications.
- Spike symbol: the `demo_wait` branch in `DemoServer::call_tool`.
- Production preview: `src/server/progress.rs`, `src/tools/dispatch.rs`, and
  `src/tools/workspace.rs`.

`demo_wait` blocks its response, emits three periodic
`notifications/progress` messages, and returns the effective request progress
token in structured content.

## 9. Narrated Client

- Design contract: migration plan, Phase 1 item 7 and validation guidance.
- Spike symbol: `src/bin/demo_client.rs`.
- Production preview: `src/mcp_client.rs` and
  `crates/bro-harness/src/mcp.rs`.

The binary uses `serve_with_lifecycle` in `Auto`, `Discover`, and legacy
`Initialize` modes. It walks through scope filtering, cache hints, resources,
listen notifications, tasks, polling, MRTR automatic and manual rounds,
progress, and strict legacy plain-JSON task fallback.

## rmcp 3.1 API Friction

1. `NeverSessionManager` and working legacy sessions are mutually exclusive,
   even when `with_legacy_session_mode(true)` is set.
2. `SubscriptionFilter` has no task IDs or task notification category.
3. `SubscriptionSink::send` rejects `notifications/tasks`.
4. The client `Subscription` also rejects task notifications associated with
   the listen request.
5. The client peer overwrites a caller-supplied progress token with its
   internal `ProgressTokenProvider` value before sending the request.
6. `ServerInfo` and `ClientInfo` are non-exhaustive aliases, so constructors
   and fluent setters are required instead of struct literals.
7. Modern lifecycle metadata validation is strict and failures appear as
   `-32602` when a client advertises 2026-07-28 without the required request
   metadata.
