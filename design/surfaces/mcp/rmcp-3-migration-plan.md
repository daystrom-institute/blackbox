---
title: "rmcp 3.0 Migration Plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - surfaces
  - mcp
brief: "Phased plan for migrating the daemon and bro-harness from rmcp 1.4 to rmcp 3.0 (MCP 2026-07-28): coupling inventory, breaking-change mapping, phase mechanics, validation."
---

# rmcp 3.0 Migration Plan

Companion to [MCP 2026-07-28 Target Surface](mcp-2026-07-28-target-surface.md),
which owns the WHAT (target shapes, decisions, open questions). This doc owns
the HOW: the concrete breaking-change inventory for our codebase, the phase
mechanics, and per-phase validation.

## Scope

- From: rmcp 1.4 (workspace-wide; root crate + `bbox-*` library crates +
  `bro-harness`).
- To: rmcp 3.0.x (3.0.1 or later; 3.0.0 shipped 2026-07-28, 3.1.0 on
  2026-07-31 adds conformance fixes worth taking).
- We skip the entire 2.x line. Most 2.x deprecations are removals in 3.0,
  and we already use the modern names (`CallToolRequestParams`, `ErrorData`,
  `*RequestParams`), so the skip is net favorable.
- MSRV becomes Rust 1.88. Our `Cargo.toml` declares no `rust-version`; add
  `rust-version = "1.88"` with the bump and confirm the lane toolchain
  satisfies it.

## Coupling inventory

| Coupling | Where | 3.0 impact |
| --- | --- | --- |
| `StreamableHttpService` + `LocalSessionManager`, `with_stateful_mode(true)`, keepalive from `BBOX_MCP_SESSION_KEEPALIVE_SECS` | `src/server/mcp.rs` (`build_http_app`) | `with_stateful_mode` renamed to `with_legacy_session_mode`; `NeverSessionManager` available for stateless |
| Per-session OnceLocks (`surface`, `surface_project`, `session_checkout`) pinned at `initialize` | `src/server/handler.rs`, `src/server/state.rs` | The one hard collision. No `initialize` in 2026-07-28; fresh handler per request. Phase 1 rework |
| Manual `ServerHandler` impl (`call_tool` -> `tool_router.call`, surface-filtered `list_tools`/`get_tool`) | `src/server/handler.rs` | Return types widen to `CallToolResponse` etc. (`.into()`); surface filtering logic itself is version-agnostic |
| 177 `#[tool]` macros across `src/tools/*.rs` | `src/tools/` | Free: macro users need no MRTR changes |
| Progress notifications (`context.meta.get_progress_token()`, `peer.send_notification(ProgressNotification)`) | `src/server/progress.rs`, `src/tools/dispatch.rs`, `src/tools/workspace.rs` | Survives (request-scoped progress stays on the response stream). `Meta` -> `RequestMetaObject` rename |
| App-level cancellation (`bro_cancel` = SIGTERM + store transition) | `src/tools/dispatch.rs` | No protocol cancellation for tools today; tasks extension adds `tasks/cancel` in Phase 2 |
| Client: workflow `mcp_call` op (stdio + streamable HTTP self-call), `().serve(transport)` | `src/mcp_client.rs` | `serve()` still works (legacy lifecycle); Phase 1 upgrades to `serve_with_lifecycle(Auto)` |
| Client: harness child -> daemon, `StreamableHttpClientTransport` | `crates/bro-harness/src/mcp.rs` | Same; the one client pair we own end to end (proving ground) |
| 80KB response cap + spill envelope | `src/server/response.rs` | Orthogonal while loopback, but the spill-to-daemon-disk rationale ("every client has file-read tools") is a localhost assumption. Phase 4 serves spills as `blackbox://spill/{id}`; required before the corpus daemon goes remote |
| Capabilities: `enable_tools()` only; no resources/prompts/subscriptions | `src/server/handler.rs` (`get_info`) | Greenfield for Phase 4 resource projection |
| `StreamableHttpService<S, M>` bound | `src/server/mcp.rs` | Now requires `S: ServerHandler` (was `Service<RoleServer>`); we already impl `ServerHandler` |
| Zero auth (loopback trust) | transport layer, implicitly | Fine until the corpus daemon leaves the machine; Q8 picks the auth story (bearer + TLS minimum) before locality-first slice 6 |

Not affected: OAuth (loopback daemon, no auth; the 3.0 OAuth rework is
skipped entirely), roots/sampling/logging (never used), SSE resumability
(never used), `transport-child-process` and reqwest TLS feature set (carried
forward unchanged).

## Phase 0: mechanical 1.4 -> 3.0 bump (zero behavior change)

Goal: everything compiles green, wire head stays legacy-stateful, no
observable behavior change. Checklist:

1. Bump `rmcp` to `3.0` in the root manifest and every `bbox-*`/`bro-*`
   crate that pins it (14 manifests today). Same feature set
   (`server`, `macros`, `transport-streamable-http-server`, `client`,
   `transport-child-process`,
   `transport-streamable-http-client-reqwest`,
   `reqwest-tls-no-provider`).
2. Add `rust-version = "1.88"` to the root package.
3. `with_stateful_mode(true)` -> `with_legacy_session_mode(true)`
   (`src/server/mcp.rs`).
4. `Meta` -> `RequestMetaObject` at the extractor and `RequestContext.meta`
   call sites (`dispatch.rs`, `workspace.rs`, `handler.rs`).
   `get_progress_token()` survives on `RequestMetaObject`.
5. Manual `ServerHandler::call_tool` returns `CallToolResponse`; wrap the
   existing `CallToolResult` with `.into()`. `ListToolsResult` gains
   optional `ttl_ms`/`cache_scope` (set in Phase 1; defaults fine here).
6. Add wildcard arms to any exhaustive matches on the six protocol unions
   (`ClientRequest`, `ServerNotification`, `ServerResult`, etc.), now
   `#[non_exhaustive]`.
7. `Annotations.last_modified` is `Option<String>` (was
   `Option<DateTime<Utc>>`) if any tool touches it; `structured_content` is
   `Option<Value>` (we emit text content, so likely no-op).
8. Result constructors now initialize `result_type: Some(COMPLETE)` and the
   server omits it for legacy peers; no action beyond snapshot updates.
9. Client call sites keep `serve()` (legacy lifecycle) in both
   `src/mcp_client.rs` and `crates/bro-harness/src/mcp.rs`.
10. Compiler-driven remainder: "the compiler is your friend" per the
    official guide. Deprecated-alias removals should not bite (we use
    modern names), but expect a tail of renames.

Validation: `cargo check`, `cargo nextest run --workspace`, `cargo clippy`,
`scripts/lint-concurrency.sh` (lane-side per the heavy-work contract). Boot a
dev daemon (`docs/operations-isolated-dev-daemon.md`) and round-trip a real
MCP client against `/mcp`. Optionally run the official MCP conformance tool
against the dev daemon (Q7).

## Phase 1: stateless-ready wire head + discover

Goal: serve 2026-07-28 stateless clients while keeping the legacy session
path for current clients. No tasks/resources yet.

1. Move surface/project resolution from `initialize` to per-request:
   extract `?surface=`/`?project=` from the `http::request::Parts` extension
   on every `get_tool`/`list_tools`/`call_tool` (and later
   `list_resources`/`read_resource`). The `SurfaceDecisionCache` already
   keys `(surface, project, generation)`; hit path is two lock reads.
2. Move checkout authority + dark overlay registration behind a shared cache
   keyed by raw selector (Q4 invalidation: generation + TTL backstop).
   Resolution must consult corpus-plane state only (project registry,
   identity stores, provisional lane), never daemon-local fs/git walks:
   locality-first decomposition removes the daemon's reach into checkouts,
   so the wire head must not bake in a filesystem the corpus daemon will
   not have. Any residual daemon-local probes needed during the overlap
   stay on the blocking pool and are marked for retirement with the
   decomposition's harness-ward moves.
3. Delete the OnceLock session pins from `BlackboxServer`; handler instances
   become stateless carriers of `Arc<SharedState>` (cheap to construct per
   request).
4. Deny semantics (Q5): evaluate the surface verdict on `server/discover`
   and per-method; keep the legacy initialize-time abort for legacy
   sessions.
5. Override `supported_protocol_versions()`; advertise both
   `V_2025_11_25` and `V_2026_07_28` behind a config gate (Q2). rmcp 3.0's
   `ProtocolVersion::LATEST` still defaults to `V_2025_11_25`, so the modern
   path is opt-in on both ends.
6. Deterministic `tools/list` ordering; set `ttl_ms` +
   `cache_scope: Private` on `ListToolsResult`.
7. Upgrade both client paths to `serve_with_lifecycle`: `Auto` mode
   (probe discover, fall back to legacy) for `mcp_call` and the harness
   child client. Third-party stdio servers (biofilter) exercise the legacy
   fallback; the daemon self-call exercises the modern path once the gate
   is on.
8. `BBOX_MCP_SESSION_KEEPALIVE_SECS` becomes legacy-only; document.

Validation: per-request scope extraction covered by unit tests at the
handler level (surface packets + `?project=` matrix, including the
gap-310c36b6 regression shape); a stateless rmcp 3.0 client fixture in
tests (client in `Discover` lifecycle mode) against an in-process server;
lane gates. Live probe: harness child dispatch round-trip in `Auto` mode.

## Phase 2: tasks extension projection

Goal: capability-gated `CreateTaskResult` from `bro_exec`/`bro_resume`;
`tasks/get` = `bro_status`; `tasks/cancel` = `bro_cancel`; `statusMessage` =
`bro_report`. Details and the dual-shape rule live in the target-surface
doc. Implementation notes:

- Adapt the orchestration `TaskStore` behind rmcp's task handler surface
  (or implement `tasks/get`/`cancel` directly over our store). Terminal
  results reuse `orch::task_result_json`.
- Cooperative cancellation maps to the existing SIGTERM path;
  `TaskContext::cancelled()` / `TaskExit` semantics where the SDK drives it.
- Task TTL aligns with existing task retention.
- Prove end-to-end daemon <-> bro-harness child first (both ends ours);
  harness children opt into the extension capability explicitly.
- `bro_wait`/`bro_status`/`bro_cancel` tools remain for legacy clients; SM
  runbooks updated to describe tasks as the accelerator for capable clients.

## Phase 3: subscriptions/listen

- `ServerHandler::listen` + `SubscriptionSink`; emit
  `notifications/tasks` on task transitions (thin payloads), driven off the
  per-task `Notify` the waiters already use.
- `toolsListChanged` on surface/packet mutation.
- `resourcesListChanged` if Phase 4 has landed.
- Harness children switch from bro_wait polling to listen + task handles;
  bro_wait remains the Tier 0 floor for all other clients.

## Phase 4: resource projection

- `blackbox://` URI scheme + resource templates per the target-surface doc.
- Surface packets gain a `resources:` dimension; `list_resources` /
  `read_resource` consult the same per-request scope extraction and
  `SurfaceDecisionCache`.
- Protocol cursor pagination + `ttl_ms`/`cache_scope` on list/read.
- Catalogs: brofiles, teams, artifacts, atoms, packets, live tasks;
  threads optional.

## Phase 5: MRTR approval gates (opportunistic)

- `InputRequiredResult` with elicitation for operator-confirmation flows
  (consultant applies, destructive admin, RX-V1 flags), gated on client
  elicitation support.
- If state ever crosses MRTR rounds statelessly, adopt rmcp's
  `request-state` feature (HMAC codec) for integrity.

## Phase ordering rationale

Phases 0-1 are one arc (the migration): everything needed to speak
2026-07-28 at all. Phases 2-3 are one arc (the orchestration surface):
the capability-gated accelerators, proven on the harness pair. Phases 4-5
are independent and can be picked up or dropped without debt.

### Interlock with locality-first decomposition

[locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md)
orders its work as: empty the daemon of checkout-coupled surfaces (slices
1-4), extract `fleetd` (slice 5), then move the corpus off-host (slice 6).
This plan interlocks at two points:

- **Slice 6 requires our Phase 1.** A remote corpus daemon cannot afford
  session affinity (restarts, LB, replicas); the 2026-07-28 stateless wire
  head plus `NeverSessionManager` is the affinity-free shape. Order:
  decomposition slices 1-4, our Phases 0-1, then slice 6.
- **Slice 6 requires the Q8 auth decision and spill-as-resource.** Zero
  auth and disk-spill recovery are loopback assumptions; both must be
  resolved (bearer + TLS minimum, `blackbox://spill/{id}`) before the
  daemon serves non-local clients. Spill-as-resource pulls a small piece
  of Phase 4 forward into the slice-6 prerequisite list.

## Client-support tripwire

Before flipping the prod version gate (Q2) or relying on any modern shape
from Claude Code, re-probe the installed binary:

```bash
BIN=~/.local/share/claude/versions/$(claude --version | awk '{print $1}')
for s in 2026-07-28 io.modelcontextprotocol/tasks subscriptions/listen server/discover; do
  echo -n "$s: "; strings -a "$BIN" | grep -c "$s"
done
```

Baseline 2026-08-03 (v2.1.220): all zero; `2025-11-25` and `Mcp-Session-Id`
present; elicitation present. When the strings appear, Claude Code has
shipped the revision.

## References

- rmcp 3.0.0 release notes (tag rmcp-v3.0.0) and migration guide
  (rust-sdk discussion #969); rmcp 3.1.0 conformance fixes
- Spec changelog 2026-07-28 and SEP list: see the target-surface doc
- `docs/operations-isolated-dev-daemon.md` for dev-daemon validation
- `src/server/CLAUDE.md` for the wire-head invariants this plan amends
- Decomposition interlock:
  [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md),
  [remote-worker-boundary.md](../../bro-harness/remote-worker-boundary.md)
