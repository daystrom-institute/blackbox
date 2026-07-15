---
title: "bbox tool projection over worker capability RPC"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - surfaces
  - daemon-runtime
tags: [rpc, capabilities, bbox-tools, stapling, policy, recursion-guard]
brief: "Projects the daemon's bbox_* MCP tools into dispatched harness sessions over the typed worker capability RPC plane instead of an MCP loopback, with daemon-side ambient-scope stapling, an explicit per-session tool grant, and recursion-guard exclusion of bro_* orchestration tools."
updated: 2026-07-15
---

# bbox tool projection over worker capability RPC

## 0. Problem

Every `bbox_*` MCP tool is unreachable from a dispatched harness session. The
transient blackbox MCP server injected into dispatched sessions
(`add_transient_blackbox_mcp_server`) is built with empty headers, so post-auth
it 401s in in-process mode, and in the production child path
(`spawn_harness_child_task`) it is not injected at all when worker RPC is
available. The result: 137 atoms and 11 workflow scripts under `system-defaults/`
that reference `bbox_note` / `bbox_search` / `bbox_knowledge` / `bbox_thread`
fail when dispatched.

The design intent (`harness-daemon-boundary.md` §6) is explicit: "Live fleet
operations use typed worker RPC, not an MCP loopback." This document specifies
the bbox projection over that typed plane.

The operator directive that shapes the whole design is **ambient context
stapling**: the daemon already knows the authenticated worker's `session_id`,
`task_id`, project root, `provider`, and bro identity. The projection staples
those server-side into projected tool calls, so a dispatched agent can neither
forget nor forge them. Fewer things for a bro to set correctly per call.

## 1. Shape

The projection reuses the existing worker capability RPC envelope
(`CapabilityRequest` / `CapabilityResponse`) rather than introducing a second
transport. A new capability family `bbox` carries the **tool name as the
operation** and the tool's JSON arguments as the bounded payload:

```text
harness ProjectedBboxTool("bbox_note")
  -> RpcCapabilityClient.call_bbox_tool
  -> CapabilityRequest { capability: "bbox", operation: "bbox_note", bounded_payload: {args} }
  -> WorkerCapabilityRouter.dispatch
       -> policy check (coarse family + fine-grained operation)
       -> ambient stapling (task/session/project/provider/bro)
       -> DaemonBboxTools.call -> BlackboxServer::bbox_note(Parameters<NoteParams>)
       -> ProjectedToolOutcome { content, is_error, structured_content, staple_overrides }
```

Typed transport, generic catalog: one RPC family and one router arm serve the
whole projected tool set; tool arguments stay schema-validated JSON handled by
the existing daemon tool layer (the daemon deserializes into the tool's real
`Parameters<T>` struct, so serde is the validator). We do not hand-type 200
tools.

### Why the tool method, not the rmcp router

Dispatching through rmcp's `ToolRouter::call` would require synthesizing a
`RequestContext<RoleServer>`, which requires a `Peer` whose constructor is
`pub(crate)` in rmcp 1.4. The bbox tool handlers only consume `&self` and
`Parameters<T>`; none read the request context. The router therefore dispatches
by calling the `#[tool]` methods directly with `Parameters(deserialize(args))`.
The cost is one match arm per projected tool (a small, explicit catalog);
slice 2 can replace the match with a schema-carrying registry if the catalog
grows past hand-maintenance.

## 2. DTOs and contract placement

Contract-bottom placement follows the boundary rules
(`harness-daemon-boundary.md` §0):

- **bro-capabilities** (`projected.rs`) owns the trait and the DTOs in its
  signature, because they cross the harness/daemon seam through a trait:
  - `BboxToolCapability { async fn call_bbox_tool(invocation_id, ProjectedToolCall) -> CapabilityResult<ProjectedToolOutcome> }`
  - `ProjectedToolCall { tool, arguments }`
  - `ProjectedToolOutcome { content, is_error, structured_content, staple_overrides }`
  - `StapleOverride { field, authoritative, supplied }`
- **bro-protocol** (`worker.rs`) owns the wire constant `CAPABILITY_BBOX = "bbox"`
  and reuses the pre-existing `SessionCapabilityPolicy.allowed_operations`
  (`BTreeMap<capability, BTreeSet<operation>>`) as the fine-grained grant shape.
  No new policy struct is needed; the bbox grant is
  `allowed_operations["bbox"] = {granted tool names}`.
- **bro-harness** (`worker/capability_rpc.rs`) implements `BboxToolCapability`
  on `RpcCapabilityClient` and registers one `ProjectedBboxTool` per granted
  name (`capabilities.rs`).
- **blackbox root crate** (`orchestration/capabilities.rs`) owns the router arm,
  the curated catalog (`PROJECTED_BBOX_TOOLS`), the stapling specs, and the
  daemon-side `DaemonBboxTools` dispatch into the tool layer.

`bro-harness` still never depends on `blackbox`; the daemon reaches the harness
only through the `bro-capabilities` trait it consumes.

## 3. Policy shape and dispatch-time grant

Two levels, both already present in the policy types:

1. **Coarse family** (`SessionPolicy.allowed_capabilities`): `"bbox"` is granted
   whenever the corpus family is granted (i.e. always, mirroring corpus). This
   is the discovery/visibility/availability projection the harness uses to
   enable or hide the tool group as a whole on reconnect.
2. **Fine-grained operation set** (`SessionPolicy` ->
   `SessionCapabilityPolicy.allowed_operations["bbox"]`): the exact set of
   projected tool names. This is the call-time authority. Both must match
   (`SessionCapabilityPolicy.allows_operation("bbox", tool)`), so a tool absent
   from the grant is rejected server-side even if a caller forges the request.

The grant is assembled at handshake in `session_policy()` (`worker_rpc.rs`) from
the curated `PROJECTED_BBOX_TOOLS` list and folded into the policy digest so the
grant is covered by the policy identity. Slice 1 curates a conservative default
catalog (see §6) rather than the full surface; §7 records the follow-up.

## 4. Ambient stapling semantics

Stapling is defined **per tool** as a set of `(json_field, StapleSource)` pairs,
where `StapleSource` resolves from the authenticated worker's ambient scope
(`WorkerCapabilityScope`, extended here with `provider` and `bro`). For each
pair, if the scope carries a value:

- if the caller supplied no value, the daemon injects the authoritative value;
- if the caller supplied a **conflicting** value, the daemon **overrides** it and
  records a `StapleOverride { field, authoritative, supplied }` in the outcome.

**Override, not reject.** We override-and-note rather than reject a conflicting
field. Rejecting would make atoms brittle: an atom that innocently passes
`project` (or copies a `task:` value from an ambient prompt) would hard-fail. The
whole point of stapling is "fewer things for a bro to set correctly," so the
server-authoritative value silently wins and the override is surfaced as an audit
annotation in the result rather than an error. The value is server-authoritative
either way; the only question is whether a conflict is fatal, and for a
projection whose goal is ergonomics it should not be.

`bbox_note` is the demonstrating tool: it staples `task_id`, `session_id`,
`project`, `provider`, and `bro` from the ambient scope. `NoteParams` already
has exactly these fields, so a dispatched agent's note is correctly attributed
to its dispatch without the agent copying identity out of its prompt.

Read/query tools (`bbox_search`, `bbox_knowledge`) have **no** staple spec: their
`project` field is a cross-corpus filter, not a write scope, and forcing it would
change query semantics. They ride the generic path unmodified, which is exactly
what proves the generic catalog works without stapling.

## 5. Recursion-guard interaction

The projected family is `bbox` only. `bro_*` orchestration / dispatch / control
tools are a different family and are **never** in `PROJECTED_BBOX_TOOLS`. Two
defenses:

1. The catalog builder rejects any name beginning with `bro_` (belt and
   suspenders against a future editing mistake), so `bro_exec` can never enter
   the grant.
2. The router only recognizes the curated operations; an ungranted operation is
   `Unauthorized` server-side.

`bro_report` remains reachable by dispatched agents through the existing harness
telemetry path, unchanged, exactly as the mechanical recursion guard already
allows. The bbox projection neither adds nor removes any `bro_*` reachability.

## 6. Slice 1 scope

Implemented and proven end-to-end:

- Curated catalog `PROJECTED_BBOX_TOOLS`: `bbox_note`, `bbox_notes`,
  `bbox_note_resolve`, `bbox_search`, `bbox_knowledge`.
- Stapling on `bbox_note` (`task_id`, `session_id`, `project`, `provider`,
  `bro`).
- `bbox_search` and `bbox_knowledge` prove the no-staple generic path.
- Fail-closed: an ungranted tool is absent from the harness tool list AND
  rejected server-side if the request is forged.
- Recursion-guard exclusion: `bro_exec` is never projectable.
- End-to-end integration through the real `serve_connection` worker RPC path.

The projected tools register with a permissive object input schema plus a
description that points at the identical `bbox_*` MCP tool. The daemon performs
the real schema validation by deserializing into the tool's `Parameters<T>`.
Atoms and workflows that call these tools already know the argument shapes, so a
permissive client-side schema is sufficient for slice 1.

## 7. Slice 2 and beyond

- **Schema fidelity.** Carry each projected tool's real JSON input schema
  (available daemon-side from `tool_router.get(name)`) to the harness so the
  model sees precise parameter schemas, not a permissive object. Keep the policy
  bounded (schemas are larger than names); a separate bounded schema channel or
  a lazy fetch is preferable to inflating the handshake policy.
- **Full catalog.** Widen `PROJECTED_BBOX_TOOLS` toward the whole `bbox_*`
  surface. Past ~64 operations per family the `SessionCapabilityPolicy` bound
  (and the match arm hand-maintenance) needs a registry-driven dispatch instead
  of an explicit match. Decide the read/write/coordination split deliberately
  rather than granting everything.
- **Surface integration.** The dispatch path already computes a surface for a
  session (`dispatch_mcp_url_for_origin`, surface packet machinery). Slice 1
  grants a conservative default catalog independent of the surface. Slice 2
  should intersect `PROJECTED_BBOX_TOOLS` with the session's evaluated surface
  so direct, code-mode, RPC-backed, and MCP-backed projections share one
  effective policy (`harness-daemon-boundary.md` §6). Until then, the projected
  set is a fixed conservative subset, which is strictly narrower than any
  reasonable surface.
- **Reconnect visibility.** Slice 1 registers the bbox catalog from the initial
  welcome policy and relies on call-time server authorization for
  fail-closed after a revocation. Per-tool visibility toggling on reconnect
  (the corpus/atom families' `ServicePolicyUpdate` machinery, extended to the
  bbox catalog names) is a refinement.
- **Retire the MCP loopback injection.** Once the projection covers the tools
  atoms actually use, `add_transient_blackbox_mcp_server` can be removed from
  the dispatch path entirely rather than left as dead-when-worker-RPC code.

## 8. fleetd mirror requirement

`fleetd` has a parallel capability broker (`crates/fleetd/src/capability.rs`)
that will, in the four-plane topology, own the worker's single service
relationship and route corpus calls to blackboxd. This slice deliberately stays
in the blackboxd / orchestration plane. When fleetd takes over capability
routing it must mirror the same contract:

- forward the `bbox` capability family and per-tool operation grant through its
  policy envelope to the blackboxd corpus client;
- perform the ambient-scope stapling on the **fleetd** side from the
  authenticated session (fleetd owns the worker session identity), or forward an
  authenticated scope the blackboxd corpus service re-staples and re-checks
  (downstream services must recheck the authorization envelope, per
  `worker.rs` `CapabilityAuthorization`; a coarse label is never enough);
- preserve the recursion-guard exclusion (no `bro_*` in the projected set);
- keep the override-and-note conflict semantics identical so an atom behaves the
  same whether routed by blackboxd (now) or fleetd (later).

This document is the source of truth for that mirror; fleetd's capability broker
should not reinvent the envelope, the grant shape, or the stapling policy.

## 9. Verification

- Policy fail-closed: ungranted tool absent from the harness registration AND
  rejected `Unauthorized` server-side when forged.
- Stapling override: a caller-supplied conflicting `project` / identity field is
  overridden and reported in `staple_overrides`.
- Recursion-guard exclusion: `bro_exec` (and any `bro_*`) is never in the
  catalog and never dispatchable.
- End-to-end: a `bbox` capability request over the real worker RPC
  (`serve_connection`) returns a successful `ProjectedToolOutcome`.
