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
context and evaluated by the same packet-style routing machinery used by
webhooks, pollers, crons, workflow gates, and hook gates.

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

The URL selector is only input. A packet decides what that selector means.

## Non-Goals

- Do not replace existing `McpFilters`. Surfaces compile down to filters.
- Do not add another bespoke allow/disallow registry if packets can express the
  routing decision.
- Do not treat `list_tools` filtering as sufficient enforcement. `call_tool`
  must reject calls outside the selected surface.
- Do not make data/project scope implicit in the tool surface. Tool visibility
  and data scoping are related but separate boundaries.

## Existing Pattern To Reuse

The current event inlet pipeline is:

```text
raw inlet payload
  -> Extractor
  -> rule packet
  -> resolve_entity_template
  -> typed verdict
  -> terminal action
```

Implemented examples:

- webhooks: signed POST -> extractor -> routing packet -> `RoutingVerdict`
- pollers: HTTP fetch -> extractor -> routing packet -> `RoutingVerdict`
- crons: schedule tick -> payload entity -> routing packet -> `RoutingVerdict`
- workflows: node output / flattened context -> gate or policy packet
- hooks: flattened context -> `when` packet -> run or skip hook

MCP surfaces should follow the same shape:

```text
MCP session/request context
  -> surface entity
  -> surface routing packet
  -> ToolSurfaceVerdict
  -> McpFilters + call enforcement
```

This keeps routing rules auditable, replayable, composable, and versioned
through the existing packet store.

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
surface input. The installed surface packet, or the built-in migration fallback,
decides what `default` means.

## Surface Entity

The packet receives a flat JSON entity. Initial shape:

```json
{
  "surface": "readonly",
  "path": "/mcp",
  "mcp_name": "blackbox-readonly",
  "client": "codex",
  "provider": "codex",
  "project": "/home/invidious/repos/transcript-search",
  "query_surface": "readonly"
}
```

Fields should be best-effort. The required field is `surface`.

Likely sources:

- `surface`: `?surface`, defaulting to `default`
- `path`: request URI path
- `query_*`: selected query params promoted to flat fields
- `mcp_name`: if known from registration or alias config
- `client` / `provider`: if known from headers, registration metadata, or alias
- `project`: optional project root if the alias is project-bound

Avoid passing arbitrary headers wholesale into the packet entity. Follow the
webhook precedent: preserve only fields that carry routing signal and are safe
to expose in replay/debug output.

## Surface Packet

Use normal packet storage. Suggested domain:

```text
mcp-surface/routing
```

Suggested packet id reference:

```text
domain:mcp-surface/routing
```

Surface packet resolution should mirror existing global/project overlay
semantics. A project-scoped surface packet overrides or supersedes the global
packet for requests bound to that project; otherwise the global packet applies.
Do not invent a second precedence model for surfaces.

Suggested lattice:

```json
["tool_surface", "deny"]
```

Example:

```json
{
  "domain": "mcp-surface/routing",
  "version": 1,
  "scope": "global",
  "classification_lattice": ["tool_surface", "deny"],
  "prefix_inference": {},
  "rules": [
    {
      "id": "readonly_surface",
      "classification": "tool_surface",
      "antecedent": { "op": "Eq", "field": "surface", "value": "readonly" },
      "consequent": "{\"route\":\"tool_surface\",\"allow\":[\"mcp__blackbox__bbox_search\",\"mcp__blackbox__bbox_context\",\"mcp__blackbox__bbox_messages\",\"mcp__blackbox__bbox_session\",\"mcp__blackbox__bbox_sessions_list\",\"mcp__blackbox__bbox_stats\"],\"disallow\":[\"mcp__blackbox__bro_*\",\"mcp__blackbox__bbox_forget\"]}"
    },
    {
      "id": "ops_surface",
      "classification": "tool_surface",
      "antecedent": { "op": "Eq", "field": "surface", "value": "ops" },
      "consequent": "{\"route\":\"tool_surface\",\"allow\":[\"mcp__blackbox__bro_*\",\"mcp__blackbox__bbox_*\"],\"disallow\":[\"mcp__blackbox__bbox_refactor_apply\"]}"
    },
    {
      "id": "default_surface",
      "classification": "tool_surface",
      "antecedent": { "op": "Eq", "field": "surface", "value": "default" },
      "consequent": "{\"route\":\"tool_surface\",\"disallow\":[\"mcp__blackbox__bro_*\",\"mcp__blackbox__bbox_refactor_*\"]}"
    },
    {
      "id": "deny_unknown_surface",
      "classification": "deny",
      "antecedent": { "op": "True" },
      "consequent": "{\"route\":\"deny\",\"reason\":\"unknown MCP surface\"}"
    }
  ]
}
```

This is intentionally the same style as `webhook-routing/forgejo` and
`webhook-routing/slack`: packet classification chooses a typed terminal action.

Surface verdicts do not automatically merge the dispatch recursion guard. The
packet owns direct MCP visibility. If `default` should hide `bro_*` or
`bbox_refactor_*`, the default rule should say so explicitly. Dispatch-time
recursion protection for spawned bros remains in `resolve_dispatch_filters`.

## ToolSurfaceVerdict

Add a small typed verdict family instead of overloading `RoutingVerdict`, because
the terminal action is not arc dispatch.

```rust
#[serde(tag = "route", rename_all = "snake_case")]
enum ToolSurfaceVerdict {
    ToolSurface {
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        disallow: Vec<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    Deny {
        #[serde(default)]
        reason: Option<String>,
    },
}
```

Evaluation should reuse:

- `packets::apply_packet_with`
- `routing::resolve_entity_template`
- the same parse-and-fail-closed posture as routing packets
- `McpFilters::merge_from`
- `normalize_filter_pattern`
- `expand_pattern`

Do not encode `deny` as empty `McpFilters`. Empty filters currently mean
"unrestricted by filters." Denial is a separate verdict. The target behavior is
to fail MCP initialization with an error such as `tool surface denied: <reason>`;
returning an empty `list_tools` response is not enough because clients treat an
empty tool catalog as a valid but tool-less server.

Unparseable surface verdicts also fail closed: treat them as
`Deny { reason: "verdict parse error" }` and fail MCP initialization rather
than falling through to empty filters.

## Enforcement Semantics

For a selected surface:

1. Evaluate the surface packet to a `ToolSurfaceVerdict`.
2. Merge the verdict filters with baseline daemon filters.
3. Filter `list_tools` output.
4. Reject `call_tool` for hidden tools, even if the client names them directly.
5. If `rmcp` exposes a per-tool fetch method in the final seam, reject hidden
   tools there too.

Disallow wins over allow, matching `McpFilters`.

If `allow` is non-empty, only matching tools are visible/callable.

If `disallow` matches a tool, it is hidden/rejected even when `allow` also
matches.

All allow/disallow patterns are normalized with `normalize_filter_pattern`
before matching. Glob expansion uses the combined `BlackboxServer` tool-router
universe, not one router half. This keeps canonical, dotted, and Copilot-style
MCP patterns equivalent to the existing dispatch-time filter behavior.

The surface selector is fixed for the MCP session, but the verdict should be
re-evaluated for each `list_tools` / `call_tool` against the current packet
store. That keeps packet edits hot for live sessions without allowing the URL
selector itself to drift mid-session.

Unknown non-default surfaces should fail closed through the packet's catchall
`deny` rule. During migration, if no surface packet is installed, `/mcp` should
preserve today's behavior through a built-in `default` fallback.

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

## Debugging And Replay

Mirror webhook tooling eventually:

- `bro_mcp_surface(action="replay", surface="readonly", ...)`
- `bro_mcp_surface(action="list")`
- `bro_mcp_surface(action="describe", surface="readonly")`

Replay should return:

```json
{
  "entity": { "surface": "readonly" },
  "verdict_classification": "tool_surface",
  "verdict_consequent": { "route": "tool_surface", "allow": [], "disallow": [] },
  "visible_tools": ["bbox_search", "bbox_context"]
}
```

This mirrors the unified `dispatch_routed_event` debugging shape more than
webhooks specifically: entity, packet verdict, typed terminal action, resulting
effect. Packet authors need to iterate on rules without restarting providers or
guessing from logs.

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

The sugar should only synthesize a URL. The packet still owns behavior.

Dispatch-time filters still matter. When a bro is spawned with a named surface,
the same evaluated `ToolSurfaceVerdict` can be merged into `resolve_dispatch_filters`
so provider-level enforcement and daemon-level visibility agree.

## Migration Plan

1. Add `ToolSurfaceVerdict` and a pure evaluator:
   `evaluate_tool_surface(packet_store, entity) -> ToolSurfaceDecision`.
2. Add filter helpers:
   `tool_visible(tool_name, decision)` and `filter_tools(tools, decision)`.
3. Prove the `rmcp` handler seam: hand-written `ServerHandler` or macro-compatible
   overrides for `list_tools` and `call_tool`.
4. Implement the proven seam around the combined `tool_router`.
5. Wire URL/query context into the evaluator at MCP session initialization.
6. Wire named surface evaluation into `resolve_dispatch_filters` so spawned bros
   can merge the surface decision into the `extra` filter layer. Disallow-wins
   composition with brofile, project, global, and dispatch filters is preserved.
7. Add built-in fallback for `surface=default` that preserves current `/mcp`
   behavior when no surface packet exists.
8. Add a sample `examples/.../packets/mcp-surface-routing.json`.
9. Add replay/describe tooling after the core enforcement path is stable.
10. Extend `bro_mcp` alias ergonomics and self-registration once the URL contract
   is proven.

## Tests

Minimum tests:

- default surface without installed packet preserves current tool count.
- `readonly` packet with non-empty `allow` hides non-matching tools from
  `list_tools`.
- direct `call_tool` for a hidden tool returns an MCP error.
- disallow wins over allow.
- unknown surface with catchall `deny` fails initialization.
- replay returns entity, verdict, and visible tool names.
- provider alias registration preserves query strings in generated MCP config.
- dispatch-time merge of a surface decision produces expected provider args for
  Claude, Codex, Copilot, and Gemini.
- hidden tools are rejected through direct `call_tool`, not only hidden from
  discovery.
- `deny` fails MCP initialization instead of returning an empty tool catalog.
- canonical, dotted, and Copilot-style allow/disallow patterns normalize to the
  same visibility decision.
- surface selection is fixed for a session after initialize.

## Open Questions

- ~~Confirm the exact `rmcp` seam for getting `?surface` into session state.~~
  **Resolved** — see Addendum below.
- Should surface decisions include instructions, and if so where can RMCP expose
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
4. Surface re-evaluation on each `list_tools`/`call_tool` (hot packet edits)
   reads `self.surface` against the current packet store — the URL selector is
   fixed but the packet result is live.
5. The `deny` verdict fails `initialize` with an MCP protocol error, not an
   empty tool list. This was the subject of note-90686e66; the doc's
   Enforcement Semantics and ToolSurfaceVerdict sections already say
   "fail MCP initialization" — the note was filed against an earlier draft.

**Superseded open question:** The first Open Question above ("Confirm the
exact `rmcp` seam") is resolved by this decision. The seam is axum middleware
`Extension` injection into the `StreamableHttpService` factory closure.
