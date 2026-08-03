---
title: "MCP 2026-07-28 Target Surface"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - surfaces
  - mcp
brief: "Target shape for the Blackbox MCP surface under the 2026-07-28 protocol revision: stateless wire head, tasks projection, subscriptions/listen wake-on-done, resource catalogs, cache hints, MRTR approval gates."
---

# MCP 2026-07-28 Target Surface

This doc defines WHAT the Blackbox MCP surface becomes under the 2026-07-28
protocol revision (rmcp 3.0). The companion plan
[rmcp 3.0 Migration Plan](rmcp-3-migration-plan.md) defines HOW we get there
(coupling inventory, breaking changes, phase mechanics, validation).

## Context

The 2026-07-28 spec revision (implemented by rmcp 3.0.0, released 2026-07-28)
is the largest protocol break since Streamable HTTP:

- Protocol-level sessions removed: no `Mcp-Session-Id`, no
  `initialize`/`initialized` handshake, no GET stream, no `Last-Event-ID`
  resumption. Every request self-describes via `_meta` (protocol version,
  client capabilities, client info). Servers expose `server/discover`
  (SEP-2567, SEP-2575).
- Experimental core tasks replaced by the `io.modelcontextprotocol/tasks`
  extension (SEP-2663): `CreateTaskResult` handles, `tasks/get` polling,
  `tasks/cancel`, `tasks/update`, `notifications/tasks`.
- `subscriptions/listen` replaces the GET stream and
  `resources/subscribe`/`unsubscribe`: one long-lived POST-response stream
  with opt-in notification classes (SEP-2575).
- Multi Round-Trip Requests (MRTR, SEP-2322): server-initiated input requests
  become `InputRequiredResult` responses; clients retry the original request
  with `inputResponses`. All results carry a `resultType` discriminator.
- Cache hints (SEP-2549): `ttlMs` + `cacheScope` on list/read results;
  deterministic `tools/list` ordering is a SHOULD for prompt-cache
  friendliness.
- `ping`, `logging/setLevel`, `roots/list_changed` removed; Roots, Sampling,
  Logging deprecated. `structuredContent` relaxed to any JSON value;
  schemas loosened to full JSON Schema 2020-12 (SEP-2106).

### Client landscape (verified 2026-08-03)

Anthropic authored the spec revision, so Claude Code support is a when, not
an if. Today's installed binary does not have it. Probe against
`~/.local/share/claude/versions/2.1.220` (built 2026-07-24, four days before
the spec cut):

- `2025-11-25` present (3 hits); `2026-07-28` absent (0 hits).
- `io.modelcontextprotocol/tasks`, `subscriptions/listen`,
  `server/discover`: 0 hits each.
- `Mcp-Session-Id` handling present; elicitation present (2025-11-25 form).

**Tripwire**: re-run this strings probe on each Claude Code update. When
`2026-07-28` and the extension method strings appear, the prod config gate
(migration plan, version advertisement) flips. The bro-harness child pair is
the only client we own end to end and is the proving ground for every modern
surface before that flip.

## Design principles

1. **Additive, capability-gated.** Every modern shape is offered only to
   clients that declare support (protocol version, extension capability).
   Legacy clients see today's surface verbatim. No flag days.
2. **Tools stay the agent action contract.** All SM runbooks teach `bro_*`
   tool flows; agents reliably call tools. Protocol-native shapes
   (tasks, resources) are projections that accelerate capable clients, not
   replacements that fork the contract.
3. **Resources are the browse plane.** Catalogs with durable IDs and JSON
   bodies (brofiles, teams, artifacts, atoms, packets, live tasks) get
   URI-addressable read projections with protocol cursor pagination. Writes
   stay tools; MCP resources are read-only, which matches our mutation
   tools' existing audit/gating shape.
4. **Scope rides the transport, not the protocol.** MCP has no scoping
   primitive (OAuth scopes gate access, not shape, and do not apply to a
   loopback daemon). Surface/project context is transport context the client
   pins at config time and every request carries.

## Stateless wire head and per-request context

### The collision

Today's wire head pins `surface`, `surface_project`, and `session_checkout`
in per-session OnceLocks at `initialize`, extracted from `?surface=` /
`?project=` query params (`src/server/handler.rs`). Under 2026-07-28 there
is no `initialize`, and rmcp 3.0 builds a fresh handler per request. Handler-
held session state must move out.

### The resolution

The query params were never session data. The rmcp streamable HTTP client
stores the configured URI verbatim and POSTs to it for every request, every
method, legacy or stateless (verified in the 1.4 client source:
`streamable_http_client.rs` keeps `uri: Arc<str>` and reuses it). The
initialize hook read per-request data once and froze it; the stateless head
reads per-request what was already arriving per-request. Every handler's
`RequestContext` exposes the same `http::request::Parts` extension the
initialize hook uses today, for every method including `resources/list` and
`resources/read`.

Scope channels, ranked by client reach:

1. **URL query params** (canonical, today's mechanism). Works with every
   client that lets you configure a URL, needs zero client cooperation.
2. **Static headers** (`headers:` map in client config). Also rides every
   request; no advantage for surface/project, available if ever needed.
3. **Custom `_meta` keys**. Spec-legal (unknown keys round-trip) but only
   where we control request construction: bro-harness children. Possible
   harness-internal lane later, never the primary.

### Consequences

- The `SurfaceDecisionCache` is already shared and generation-keyed; the
  per-request hit path is two lock reads. Surface evaluation cost is
  unchanged in practice.
- The OnceLock session-pinning footgun class is deleted: no pinned pair to
  forget to pass (gap-310c36b6), no half-initialized session answering tool
  lists.
- Checkout authority (`resolve_project_write` + dark overlay refresh) does
  blocking fs/git probes and becomes per-request, so it needs a shared cache
  keyed by raw selector. This is a write-authority decision: staleness has a
  security flavor. Recommended: generation-keyed like `SurfaceDecisionCache`
  with a short TTL backstop. (Open question Q4.)
- Deny semantics change: with no `initialize` to abort, a denied surface
  fails per-method. Recommended: deny at `server/discover` AND per-method
  (defense in depth), so misconfiguration is loud, not a silent empty tool
  list. (Open question Q5.)

### server/discover

Implement for 2026-07-28 compliance (servers MUST). rmcp derives a default
from `get_info()`; we override `supported_protocol_versions()` and advertise
the tasks extension in `capabilities.extensions` once Phase 2 lands.
Discover is also where surface denial becomes visible early.

## Tasks projection (SEP-2663)

The orchestration `TaskStore` is already the backend the extension models:
durable IDs, terminal/non-terminal statuses, cooperative cancellation
(SIGTERM is "acknowledged, not obligated"), TTL-shaped retention, status
messages via `bro_report`.

Mapping:

| Extension shape | Blackbox today |
| --- | --- |
| `CreateTaskResult` from `tools/call` | `bro_exec` / `bro_resume` (returns handle instead of blocking) |
| `tasks/get` | `bro_status` |
| `tasks/cancel` | `bro_cancel` (cooperative both ways) |
| task `statusMessage` | `bro_report` |
| `tasks/update` | does NOT map to `bro_steer` (update answers outstanding inputRequests; steer is unsolicited mid-turn input) |

Dual-shape rule: `bro_exec`/`bro_resume` return pure
`CallToolResponse::Task(CreateTaskResult)` when the client declares the
tasks extension in per-request capabilities, and the current `{taskId,
sessionId}` JSON otherwise. The extension is server-directed per request and
a server MUST NOT return a task to a client that did not declare support, so
the gate is unambiguous and both shapes are spec-legal.

Caveats:

- rmcp 3.0's `TaskManager` scaffolds durability/TTL/cancellation, but task
  status notifications over listen are not wired in the SDK; we emit them
  ourselves (next section).
- Steer and interrupt stay tools; forcing them into `tasks/update` would
  violate the envelope's semantics.
- Scope decision (Q3): bro dispatches first. Workflow runs are equally
  task-shaped but carry richer state (node graph); expose them as tasks only
  if the bro projection proves clean, otherwise as `blackbox://run/{id}`
  resources.

## Wake-on-done: three tiers

- **Tier 0 (floor, all clients, keep forever):** `bro_wait` /
  `bro_when_all` / `bro_when_any` block the response stream with
  `notifications/progress` ticks (current mechanism,
  `src/server/progress.rs`). Request-scoped progress survives 2026-07-28
  unchanged.
- **Tier 1 (tasks extension):** poll `tasks/get` respecting
  `pollIntervalMs`. No held connection; task IDs survive client restarts.
- **Tier 2 (listen):** client opens `subscriptions/listen` once; the daemon
  emits `notifications/tasks` on task transitions. True wake-on-done with no
  polling and no held request. Implementation: `ServerHandler::listen` +
  `SubscriptionSink`, driven off the same per-task `Notify` the waiters
  already select on. The roster SSE stream proves the event source exists;
  this re-channels it in-protocol.

Notification payload policy: thin (status + statusMessage + result
availability), not full task state. Terminal results can exceed the 80KB
response cap; subscribers are waiting on "done", not the payload, and can
`tasks/get` for the body.

`toolsListChanged` rides the same stream: today a surface-packet mutation
silently changes what `list_tools` returns while clients cache the old list
forever. Emit `toolsListChanged` on surface/packet mutation and set `ttlMs`
on `ListToolsResult`; the incoherence window closes.

## Resource projection

URI scheme `blackbox://`, read-only, served under the same surface verdicts
as tools:

```
blackbox://brofile/{name}
blackbox://team/{name}
blackbox://artifact/{kind}/{name}
blackbox://packet/{id}
blackbox://atom/{id}
blackbox://task/{id}                     (live task state)
blackbox://project/{project}/packet/{id} (explicit project encoding)
```

- Catalog boundary: the five catalogs plus live tasks. Threads are
  borderline (cheap read projection, composes with subscriptions).
  Transcripts and sessions are searchable corpora, not enumerable catalogs;
  they stay tool-served.
- Project scoping is explicit in the URI for project-owned objects, not
  resolved against the session's `?project=`: a client scoped to project A
  may legitimately read project B's packets, and URI-addressability beats
  scope-channel switching.
- Governance: extend surface packets with a `resources:` dimension (same
  packet, not a separate packet type; operators think in surfaces, not
  planes). `list_resources` / `read_resource` consult the same
  per-request scope extraction and `SurfaceDecisionCache`.
- Pagination: `resources/list` and `resources/templates/list` carry protocol
  cursors, descriptor-only listing (progressive disclosure; `resources/read`
  fetches one full body), plus `ttlMs`/`cacheScope`. This relieves the
  chronic over-cap list-tool pattern (`bbox_artifact_list`,
  `bbox_describe_schema(include_agents=true)`) that the 80KB cap's bytes
  telemetry exists to flag.
- `resourcesListChanged` over listen on catalog mutation (artifact install,
  packet compile, brofile upsert).

## Cache hints and tools/list hygiene (SEP-2549)

- Deterministic `tools/list` ordering (spec SHOULD; prompt-cache
  friendliness). Stabilize `tool_router.list_all()` order.
- `ttlMs` + `cacheScope: Private` on `ListToolsResult` (surface verdicts are
  per-client context), on resource list/read results, and on prompt lists if
  ever added.
- Tool-result pagination stays app-level: protocol cursors exist only on
  list/read endpoints, not `tools/call`. For the large-response tools
  (`bro_dashboard`, `bbox_search`, `bbox_hybrid_search`), converge on a
  uniform `{items, next_cursor, total_estimate}` envelope instead of per-tool
  bespoke limit params. The 80KB cap + spill envelope stays regardless.
- structuredContent going forward: 2026-07-28 allows any JSON value, so new
  or changed tools should ship `outputSchema` + `structuredContent`. No mass
  retrofit of the 177 existing tools.

## MRTR approval gates (SEP-2322)

Operator-confirmation flows today are "dispatch, get refused, re-dispatch
with a flag" (consultant proposal applies, destructive admin ops, RX-V1
operator-authority flags). MRTR lets a tool return `InputRequiredResult`
with an elicitation; the client answers and retries the original request
with `inputResponses`. Elicitation UX already exists client-side in
2025-11-25 form, so this is a transport upgrade, not a new client capability
from scratch. Gated on client support; opportunistic phase, not a
commitment.

Stateless MRTR note: `requestState` is echoed verbatim by clients, so a
stateless server must verify integrity. rmcp's opt-in `request-state`
feature (HMAC-SHA256 codec) is the mechanism if we ever carry state across
MRTR rounds.

## Decisions recorded (operator, 2026-08-03 discussion)

1. Dual-stack work is not speculative: Anthropic authored the spec revision,
   so Claude Code support is a when, not an if. Phases 1-3 are "be ready
   before the client lands", not experiments.
2. Legacy session mode is a bridge measured in Claude Code release cycles,
   not a permanent path. Keep until the installed base moves; design treats
   legacy as temporary.
3. The bro-harness child pair is the proving ground for tasks + listen
   end-to-end before any prod capability flip.
4. Scope channel: URL query params remain canonical; resource endpoints
   inherit scope filtering from the transport (no new plumbing).

## Open questions

Recommendations stated; operator red-lines here.

- **Q1 (migration sequencing)**: Phase 0 dependency bump standalone, or
  folded with the Phase 1 stateless rework? Recommend standalone: the
  per-request-context rework touches the trust model and deserves its own
  review surface.
- **Q2 (version advertisement)**: advertise `V_2026_07_28` in discover
  immediately after Phase 1, or dark-launch behind a config gate until the
  harness loop is proven? Recommend config-gated, default on for dev daemon,
  flip prod on the strings-probe tripwire.
- **Q3 (task scope)**: bro dispatches only, or workflow runs as tasks too?
  Recommend bro first, workflow runs fast-follow or `blackbox://run/{id}`.
- **Q4 (checkout authority cache)**: invalidation for the per-request
  write-authority cache. Recommend generation-keyed + TTL backstop.
- **Q5 (deny semantics)**: discover-deny + per-method deny, or per-method
  only. Recommend both.
- **Q6 (notification payload)**: thin status vs full task state in
  `notifications/tasks`. Recommend thin (decided above unless red-lined).
- **Q7 (conformance gates)**: wire the official MCP conformance suite into
  lane verification, or run it manually per phase? Recommend manual per
  phase first; promote to gates only if it catches what nextest misses.

## References

- Spec changelog: modelcontextprotocol.io/specification/2026-07-28/changelog
- SEP-2567 (stateless HTTP), SEP-2575 (lifecycle/discover/listen),
  SEP-2663 (tasks extension), SEP-2322 (MRTR), SEP-2549 (cache hints),
  SEP-2243 (standard headers), SEP-2106 (schema relaxation),
  SEP-2260 (request association)
- rmcp 3.0.0 release (modelcontextprotocol/rust-sdk tag rmcp-v3.0.0) and
  migration guide (rust-sdk discussion #969)
- Tasks extension spec: modelcontextprotocol/ext-tasks
- Companion plan: [rmcp 3.0 Migration Plan](rmcp-3-migration-plan.md)
