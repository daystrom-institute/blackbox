---
title: "MCP Surfaces"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - mcp
---

# MCP Surfaces

Blackbox already has dispatch-time MCP filters for spawned bros:

- global and project `McpStore.filters`
- brofile filters
- per-dispatch `allow_tools` / `disallow_tools`
- provider-specific translation in `Provider::build_filter_args`
- the default recursion guard over `bro_*` and `bbox_refactor_*`

Those filters answer "what may this spawned provider call?" They do not fully
answer "what tools should this MCP caller discover in the first place?"

MCP surfaces make that discovery boundary first class. A surface is a caller
selected view of the daemon's MCP tool catalog. The view is selected by URL
context and defined by a named `[surfaces.<name>]` table in daemon
configuration.

## Goal

Expose multiple intentional MCP tool surfaces from one daemon without creating
parallel hand-maintained tool registries.

Examples:

```text
http://127.0.0.1:7264/mcp
http://127.0.0.1:7264/mcp?surface=readonly
http://127.0.0.1:7264/mcp?surface=ops
http://127.0.0.1:7264/mcp?surface=badgey
```

Provider configs can then register aliases against the same daemon:

```text
blackbox          -> http://127.0.0.1:7264/mcp
blackbox-readonly -> http://127.0.0.1:7264/mcp?surface=readonly
blackbox-ops      -> http://127.0.0.1:7264/mcp?surface=ops
```

The URL selector is only input. The configured surface table decides what that
selector means.

## Non-Goals

- Do not replace existing `McpFilters`. Surfaces compile down to filters.
- Do not treat `list_tools` filtering as sufficient enforcement. `call_tool`
  must reject calls outside the selected surface.
- Do not make data/project scope implicit in the tool surface. Tool visibility
  and data scoping are related but separate boundaries.

## Resolution Pipeline

Every surface rule matches only the surface name and yields an allow list and a
disallow list, so a surface is a lookup table rather than a rule program:

```text
?surface=<name> on initialize
  -> surfaces[name] from daemon configuration
  -> visible tool set over the served catalog
  -> list_tools / call_tool / get_tool enforcement
```

A name missing from the table is refused at `initialize`.

## URL Contract

Canonical selector:

```text
?surface=<id>
```

Path form can be added later if useful:

```text
/mcp/surface/<id>
```

The query parameter is the first implementation target because the daemon
already mounts one `StreamableHttpService` at `/mcp`.

Surface selection is session-scoped. The selector is read during MCP session
initialization; subsequent JSON-RPC frames inherit the session's surface. Query
strings on later requests must not change an established session's surface.

No selector means:

```text
surface = "default"
```

That is not "unscoped all tools" as a semantic concept. It is just the default
surface input. The `default` entry in the configured surface table decides what
`default` means.

Surface selection is independent of `?project`. The project parameter sets the
session's project context for data scoping; it does not select or modify the
tool surface.

## Surface Configuration

Surfaces are daemon configuration. Built-in surfaces ship in
`crates/bbox-config/src/default_surfaces.toml`. A `[surfaces.<name>]` table in
the daemon config file overrides the built-in surface of the same name or adds
a new one:

```toml
[surfaces.default]
disallow = ["bbox_project_*", "bbox_storage_*"]

[surfaces.reviewer]
allow = ["bbox_hybrid_search", "bbox_inspect_entity", "bbox_knowledge"]
```

Each table has two keys, both optional lists of glob patterns over tool names:

- `allow`: when non-empty, only matching tools are visible.
- `disallow`: matching tools are hidden, even when `allow` also matches.

A table with neither key shows the whole served catalog. Config validation
rejects empty surface names, names with surrounding whitespace, empty
patterns, and unknown keys. The surface table loads with daemon configuration,
so a surface change applies on daemon restart.

Built-in surfaces (the tool lists are in `default_surfaces.toml`):

| Surface | Caller | Shape |
| --- | --- | --- |
| `default` | External clients and ordinary dispatches | Disallow list: maintenance, admin, provenance and artifact-install tools |
| `interactive` | Fleet cockpit sessions | Disallow list: the operator tools, including `bro_allocator_*` |
| `agent-internal` | Workflow-owned dispatches | Disallow list: dispatch lifecycle tools (`bro_exec`, `bro_resume`, `bro_cancel`, `bro_prune`) plus the `default` maintenance and admin set |
| `readonly` | Reviewers, evaluators and observers | Allowlist of read-only retrieval and status tools |
| `ops` | Operators | Empty table: the full catalog |

Tools hidden from agent-facing surfaces stay reachable through `ops`; operators
run them with `bro mcp call <tool> '<json>' --surface ops`.

Surfaces do not automatically merge the dispatch recursion guard. The surface
table owns direct MCP visibility. If `default` should hide `bro_*` or
`bbox_refactor_*`, its table says so explicitly. Dispatch-time recursion
protection for spawned bros remains in `resolve_dispatch_filters`.

Matching reuses `normalize_filter_pattern` and `glob_match` from the dispatch
filter code.

## Refusal

Do not encode an unknown surface as empty `McpFilters`. Empty filters mean
"unrestricted by filters." A surface name missing from the configured table
fails MCP initialization with `tool surface denied: unknown MCP surface:
<name>`; returning an empty `list_tools` response is not enough because
clients treat an empty tool catalog as a valid but tool-less server.

## Enforcement Semantics

For a selected surface:

1. Look up the surface in the configured table at `initialize`.
2. Compute the visible tool set from its `allow`/`disallow` patterns over the
   served catalog, and store it on the session.
3. Filter `list_tools` output.
4. Reject `call_tool` for hidden tools, even if the client names them directly.
5. Return nothing from `get_tool` for hidden tools.

Disallow wins over allow, matching `McpFilters`.

If `allow` is non-empty, only matching tools are visible/callable.

If `disallow` matches a tool, it is hidden/rejected even when `allow` also
matches.

All allow/disallow patterns are normalized with `normalize_filter_pattern`
before matching. Glob expansion uses the combined `BlackboxServer` tool-router
universe, not one router half. This keeps canonical, dotted, and Copilot-style
MCP patterns equivalent to the existing dispatch-time filter behavior.

The visible tool set is computed once at `initialize` and held in a per-session
`OnceLock`; `list_tools`, `call_tool` and `get_tool` read it without further
evaluation. Both the selector and the surface table are fixed for the life of
the session.

Unknown surfaces fail closed at `initialize`. Configuration can override but
not remove a built-in surface, so a session without a selector always resolves
to `default`.

## Implementation Seam

The daemon currently mounts:

```rust
.nest_service("/mcp", mcp_service)
```

and `BlackboxServer` uses an `rmcp::ToolRouter<Self>`.

`rmcp`'s generated `#[tool_handler]` implementation is a feasibility seam, not
an implementation detail to hand-wave. Today it generates the handler methods
around the combined `self.tool_router`; explicit overrides may not compose with
the macro. The implementation spike must prove one of these paths:

1. Drop `#[tool_handler]` for `BlackboxServer` and hand-write
   `ServerHandler` over the combined router.
2. Find a macro-supported way to override only the needed methods without
   losing the generated tool-call plumbing.

The methods that need surface enforcement are:

- `list_tools`
- `call_tool`

If the final `rmcp` seam exposes `get_tool`, it should be filtered too. Do not
invent a separate per-tool fetch path solely for this feature; `call_tool`
enforcement is the mandatory execution boundary.

The implementation can keep the existing router but wrap its catalog and calls:

```text
self.tool_router.list_all()
  -> filter by selected surface

self.tool_router.call(context)
  -> visible? call : error
```

The selected surface must be captured at MCP session initialization. `rmcp` 1.4's
`StreamableHttpService::new` constructor closure does not receive URL query
parts directly, so URL context plumbing is part of the spike. Viable paths:

1. Wrap the streamable HTTP service in axum middleware that resolves `?surface`
   and stores the selected surface where the session handler can read it.
2. Create one `StreamableHttpService` per mounted surface path.

Query param support is preferred because it keeps the URL as routing data. The
path-per-surface fallback is acceptable for v1 if `rmcp` makes query capture
too invasive.

## Inspection

A surface's definition is its configuration table; there is no runtime surface
tool. Its effect is observable directly: `tools/list` on a session opened with
`?surface=<name>` returns exactly the visible set.

## Provider Registration

`bro_mcp action=add` can grow optional surface alias support:

```json
{
  "action": "add",
  "name": "blackbox-readonly",
  "url": "http://127.0.0.1:7264/mcp?surface=readonly"
}
```

Later sugar:

```json
{
  "action": "add_surface",
  "name": "blackbox-readonly",
  "surface": "readonly"
}
```

The sugar should only synthesize a URL. The surface table still owns behavior.

Dispatch-time filters still matter. When a bro is spawned with a named surface
(a brofile `surface` selector or a dispatch path's surface),
`dispatch_surface_filters(&config.surfaces, surface)` folds the configured
surface into the child's tool filters so provider-level enforcement and
daemon-level visibility read the same table. An unknown surface maps to a
deny-all filter, matching the wire head's refusal. Disallow-wins composition
with brofile, project, global, and dispatch filters is preserved.

## Implementation Steps

1. Add `SurfaceConfig { allow, disallow }` and the built-in table to
   `bbox-config`; merge `[surfaces.<name>]` tables from the daemon config file
   over it by name.
2. Add `visible_tool_set(surface, universe)` over the served catalog.
3. Prove the `rmcp` handler seam: hand-written `ServerHandler` or macro-compatible
   overrides for `list_tools`, `call_tool` and `get_tool`.
4. Implement the proven seam around the combined `tool_router`.
5. Read `?surface` at MCP session initialization, refuse unknown names, and
   store the visible set on the session.
6. Fold named surfaces into dispatch filters through `dispatch_surface_filters`.
7. Extend `bro_mcp` alias ergonomics and self-registration once the URL contract
   is proven.

## Tests

Minimum tests:

- default surface preserves its configured tool count.
- a surface with non-empty `allow` hides non-matching tools from `list_tools`.
- direct `call_tool` for a hidden tool returns an MCP error.
- disallow wins over allow.
- unknown surface fails initialization.
- config tables override built-in surfaces by name and add new ones.
- provider alias registration preserves query strings in generated MCP config.
- dispatch-time merge of a surface produces expected provider args for
  Claude, Codex, and Copilot.
- hidden tools are rejected through direct `call_tool`, not only hidden from
  discovery.
- an unknown surface fails MCP initialization instead of returning an empty
  tool catalog.
- canonical, dotted, and Copilot-style allow/disallow patterns normalize to the
  same visibility decision.
- surface selection is fixed for a session after initialize.

## Open Questions

- ~~Confirm the exact `rmcp` seam for getting `?surface` into session state.~~
  **Resolved** — see Addendum below.
- Should surfaces carry instructions, and if so where can RMCP expose
  per-surface instructions cleanly?
- Should surface aliases be auto-registered by default, or only through explicit
  `bro_mcp` calls?
- Should a surface be allowed to change data scope defaults for tools that accept
  project parameters, or should that remain strictly out of scope?

## Addendum: Surface-Binding Model Decision

**Decision: session-level surface binding (option a).**

Rationale grounded in rmcp 1.4 mechanics: `StreamableHttpService` uses
`LocalSessionManager`. The factory closure that constructs each
`BlackboxServer` instance fires once at `initialize` and receives only the
initial HTTP request. Subsequent JSON-RPC frames (`list_tools`, `call_tool`,
`notifications/cancelled`, etc.) travel over the established session channel
and never re-expose URL query parameters. Per-request surface variation is
therefore not mechanically available through `LocalSessionManager` — the
closure has already returned and the handler owns the session.

The alternative — option (b) per-request binding — would require replacing
`LocalSessionManager` with a custom session manager that re-reads URL state on
every request. That is a high-blast-radius change to the rmcp integration seam
and buys nothing over session-level binding: MCP session semantics already
treat `initialize` as the contract boundary for capability negotiation, so
varying the tool surface mid-session would contradict the protocol model
anyway.

**Chosen implementation path (option a, path 1):**

Wrap the existing `StreamableHttpService` in a thin axum `Extension`-injection
layer that reads `?surface` from the `initialize` request URI and stores it in
an `Arc<str>` (or small newtype). The `BlackboxServer` factory closure receives
this value from axum `Extension` extraction and stores it as a session-scoped
field. All subsequent `list_tools` and `call_tool` calls read from that field.

```text
GET /mcp?surface=readonly HTTP/1.1      <- URL lives here, on initialize
  -> axum middleware injects Extension<SurfaceId>
  -> factory closure reads Extension<SurfaceId>
  -> BlackboxServer { surface: "readonly", ... }
  -> all handler methods read self.surface
```

The path-per-surface fallback (option a, path 2: separate
`StreamableHttpService` mount per surface) is an acceptable v1 fallback if
axum `Extension` extraction from inside the factory closure proves impossible,
but should be the last resort because it multiplies mount points and makes
dynamic surface registration awkward.

**Constraints this decision records:**

1. `?surface` is read-once at `initialize`; no mid-session surface changes.
2. `BlackboxServer` gains a `surface: Arc<str>` field (or equivalent).
3. `LocalSessionManager` is preserved; no custom session manager.
4. The visible tool set is computed once at `initialize` and stored beside
   `self.surface`; `list_tools`, `call_tool` and `get_tool` read it.
5. An unknown surface fails `initialize` with an MCP protocol error, not an
   empty tool list.

**Superseded open question:** The first Open Question above ("Confirm the
exact `rmcp` seam") is resolved by this decision. The seam is axum middleware
`Extension` injection into the `StreamableHttpService` factory closure.
