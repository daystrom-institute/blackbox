---
title: "MCP Surfaces \u2014 Phased Implementation Plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - mcp
---

# MCP Surfaces — Phased Implementation Plan

Source design: `design/surfaces/mcp/mcp-surfaces.md`. This document breaks the implementation
into six phases (1, 1b, 2a, 2b, 3, 4), each producing a testable, mergeable
increment.

## Review Convergence Notes

Codex bro review (`codex-gpt55`, session `019e098a-40c6-75f0-a88b-219dccac0e20`)
converged on 2025-05-08 after two rounds. Key resolved findings:

1. **rmcp seam:** `#[tool_handler]` only generates methods missing from the impl
   block (verified rmcp-macros-1.4.0 `tool_handler.rs:44,64,81,91`). The
   `Service<RoleServer>` dispatch calls trait methods directly. So: keep
   `#[tool_handler(router = self.tool_router)]` and override `list_tools`,
   `call_tool`, `get_tool` in the same impl block. Do NOT drop the macro.

2. **`get_tool` filtering:** rmcp 1.4 has `ServerHandler::get_tool` (returns
   `Option<Tool>`). Must filter it alongside `list_tools`/`call_tool`. No router
   wrapping needed.

3. **Session binding:** `RequestContext` carries `extensions: Extensions`
   (rmcp-1.4.0 `service.rs:654`). `StreamableHttpService` inserts original
   `http::request::Parts` into JSON-RPC request extensions before `initialize`
   (`tower.rs:642`, `server.rs:206`). Clean v1 path: override `initialize`,
   read `Parts` from `context.extensions`, parse `?surface` from URI, store on
   `BlackboxServer`. No path-per-surface fallback needed.

4. **Allow-intersection:** `McpFilters::merge_from` appends allows (union
   semantics). This only matters for Phase 3 (dispatch path), not the direct
   MCP handler path (which evaluates a single surface decision independently).
   Phase 3 must add allow-intersection composition for the dispatch filter stack.

5. **Project-scoped packet resolution:** `Packets::load("domain:...")` picks
   newest across global/project. Wrong for surfaces. Add
   `load_latest_by_domain(domain, project: Option<&str>)` with project-first
   fallback to global semantics.

6. **Tool name canonicalization:** `ToolRouter::list_all()` returns bare rmcp
   names, but surface packet examples use `mcp__blackbox__*` prefixes. The
   evaluator must normalize both directions — patterns to bare names for matching.

## Phase 1 — Verdict types, pure evaluator, project-aware lookup

**Goal:** Introduce `ToolSurfaceVerdict`, a pure evaluator, and project-scoped
packet resolution. No changes to the MCP handler or HTTP stack.

**Scope:**

- Add `ToolSurfaceVerdict` enum in `src/server/surface.rs` (new file):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub(crate) enum ToolSurfaceVerdict {
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

- Add `ToolSurfaceDecision` wrapping the verdict + resolved `McpFilters`:

```rust
pub(crate) struct ToolSurfaceDecision {
    pub verdict: ToolSurfaceVerdict,
    pub filters: orchestration::mcp::McpFilters,
}
```

- Add `evaluate_tool_surface(state, surface_entity, project: Option<&str>) -> ToolSurfaceDecision`:
  1. Call `packets.load_latest_by_domain("mcp-surface/routing", project)`.
  2. If `Ok(None)`, return passthrough `ToolSurfaceVerdict::ToolSurface { allow: [], disallow: [], instructions: None }`.
  3. If load error (corrupted/unreadable), return `Deny { reason: "surface packet load error" }` (fail closed).
  4. If packet found, `apply_packet_with` against the entity.
  5. If `apply` returns `None` (no rule matched), return `Deny { reason: "no matching surface rule" }`.
  6. Parse consequent JSON as `ToolSurfaceVerdict`. Parse failure → `Deny`.
  7. Convert allow/disallow into `McpFilters` via `normalize_filter_pattern`.

- Add `load_latest_by_domain(domain, project: Option<&str>) -> Result<Option<Packet>>`
  on `Packets` in `src/packets/mod.rs`:
  1. Iterate newest-first.
  2. If `project` present: first match where `domain` matches AND `scope == "project"` AND `project` matches. Else fall back to newest global packet with same domain.
  3. If no project passed: use global only.

- Add `tool_visible(tool_name, decision, universe) -> bool`:
  - Deny verdict → false.
  - ToolSurface: normalize `tool_name` to bare form, expand allow/disallow
    patterns against the full tool universe, apply disallow-wins-over-allow.

- Add `filter_tools(tools: &[Tool], decision, universe) -> Vec<Tool>`.

- Extract tool universe to `SharedState::tool_universe: Vec<String>` (populated
  at daemon startup from `bbox_tools + bro_tools` router catalogs). Reuse in
  both surface evaluation and dispatch filter expansion.

- Wire `mod surface;` into `src/server/mod.rs`.

**Tests (in `src/server/surface.rs`):**

- default surface with no installed packet → empty filters (passthrough).
- `readonly` verdict with non-empty allow hides non-matching tools.
- disallow wins over allow.
- unparseable consequent → Deny fallback.
- no-match (all rules miss) → Deny fallback.
- corrupted packet load → Deny (not passthrough).
- canonical `mcp__blackbox__bbox_search`, dotted `mcp__blackbox__.bbox_search`,
  Copilot `blackbox(bbox_search)`, and bare `bbox_search` all match the same
  tool after normalization.
- project-scoped packet overrides global for matching project, global used for
  non-matching project.

**Does not touch:** `BlackboxServer`, `ServerHandler`, HTTP routing, `StreamableHttpService`.

## Phase 1b — Pure evaluator replay

**Goal:** Surface the evaluator as an MCP tool for packet authoring iteration
without needing live MCP sessions.

**Scope:**

- Add `bbox_mcp_surface` tool (or extend `bbox_artifact_list` — prefer a dedicated
  tool since the action set is specific):

| Action | Purpose |
|--------|---------|
| `replay` | Accept `{surface, client?, project?}`, build entity, evaluate packet, return `{entity, verdict_classification, verdict_consequent, visible_tools}` |

- `replay` is pure evaluation, no side effects. Mirrors `bro_webhook_replay`.

- Add `tool_docs.rs` stanza.

**Tests:**

- `replay` with `surface=readonly` → returns expected verdict + visible tool list.
- `replay` with unknown surface → returns `deny` verdict.
- `replay` with project → uses project-scoped packet when available.

## Phase 2a — rmcp handler seam with hardcoded surface

**Goal:** Override `list_tools`, `call_tool`, and `get_tool` in the existing
`#[tool_handler]` impl to enforce surface decisions. Surface is hardcoded to
`"default"` — no session binding yet.

**Scope:**

- Add `surface: OnceLock<Arc<str>>` to `BlackboxServer` in `src/server/state.rs` (set during `initialize`):
  ```rust
  pub(crate) struct BlackboxServer {
      pub(crate) state: Arc<SharedState>,
      pub(crate) tool_router: ToolRouter<Self>,
      pub(crate) surface: OnceLock<Arc<str>>,
  }
  ```
  Default to `"default"` in `new()`.

- **Keep** `#[tool_handler(router = self.tool_router)]` on the impl block.
  Override three methods:

```rust
#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo { /* unchanged */ }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Read ?surface from request URI in context extensions.
        // Store in self.surface (OnceLock — set once per session).
        // If surface evaluates to Deny, fail initialize with McpError.
        // For 2a: hardcode "default" (no URI parsing yet).
        let _ = self.surface.set(Arc::from("default"));
        // Delegate to default behavior
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = serde_json::json!({ "surface": surface });
        let decision = surface::evaluate_tool_surface(&self.state, entity, None::<&str>);
        if !surface::tool_visible(name, &decision, &self.state.tool_universe) {
            return None;
        }
        self.tool_router.get_tool(name)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
    ) -> Result<ListToolsResult, McpError> {
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = serde_json::json!({ "surface": surface });
        let decision = surface::evaluate_tool_surface(&self.state, entity, None::<&str>);
        let all = self.tool_router.list_all();
        let filtered = surface::filter_tools(&all, &decision, &self.state.tool_universe);
        Ok(ListToolsResult { tools: filtered, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = serde_json::json!({ "surface": surface });
        let decision = surface::evaluate_tool_surface(&self.state, entity, None::<&str>);
        if !surface::tool_visible(&request.name, &decision, &self.state.tool_universe) {
            return Err(McpError::method_not_found(
                format!("tool not available on surface '{}': {}", surface, request.name)
            ));
        }
        self.tool_router.call_tool(request, context).await
    }
}
```

  The exact `call_tool` / `list_tools` signatures must match rmcp 1.4's
  `ServerHandler` trait. The pseudo-code above is indicative; the actual
  method signatures come from the trait definition.

**Tests:**

- Integration test: `BlackboxServer` with default surface and no packet → full
  catalog (behavior unchanged).
- Integration test: install surface packet, verify `list_tools` returns filtered
  set.
- Integration test: `call_tool` for hidden tool returns MCP error.
- Integration test: `get_tool` for hidden tool returns `None`.
- Integration test: `deny` verdict fails `initialize` (not empty catalog).
- All existing `cargo test` passes (backward compatibility gate).

**Merge gate:** all existing tests pass. Surface defaults to `"default"` with
no packet installed, so behavior is identical to pre-surface code.

## Phase 2b — Session binding from RequestContext

**Goal:** Read `?surface` from the `initialize` request URI via rmcp's
`RequestContext` extensions. Store on the session's `BlackboxServer`.

**Scope:**

- Implement `initialize` override to:
  1. Extract `http::request::Parts` from `context.extensions`.
  2. Parse `parts.uri.query()` for `surface` parameter.
  3. Default to `"default"` if absent.
  4. Evaluate the surface decision immediately. If `Deny`, fail `initialize`
     with `McpError::internal_error("tool surface denied: <reason>")`.
  5. Set `self.surface` via `OnceLock`.

- Optionally add `SurfaceId` newtype for type-safe extraction from the URI
  query, but since we're reading from `http::request::Parts` directly (not axum),
  a simple string extraction is sufficient.

**Tests:**

- Initialize with `?surface=readonly` → session sees filtered `list_tools`.
- Initialize without `?surface` → `"default"` surface, full catalog.
- Initialize with unknown surface → `initialize` returns MCP error.
- Subsequent requests on the same session ignore a different `?surface` in the
  URI (OnceLock already set).
- Two concurrent sessions with different surfaces each keep their init-time
  surface.

## Phase 3 — Dispatch integration and provider registration

**Goal:** Surface decisions compose into `resolve_dispatch_filters` so spawned
bros inherit the correct tool boundary. Provider configs gain surface aliases.

**Scope:**

- Extend `resolve_dispatch_filters` in `src/server/progress.rs` to accept `surface: Option<&str>`:
  - When present, evaluate the surface packet and merge the resulting
    `McpFilters` into the effective filter set.
  - **Allow-intersection fix:** `McpFilters::merge_from` currently appends
    allows (union). For the surface layer, allow patterns must intersect with
    the existing allow set (if any). Add `intersect_from(&mut self, other)` or
    handle intersection logic inline in `resolve_dispatch_filters` when the
    surface layer is present: expanded allow = intersection of existing expanded
    allow and surface expanded allow.
  - Insert surface filters between recursion guard and per-dispatch `extra`.
  - Disallow remains additive (append).

- Update all call sites of `resolve_dispatch_filters`:
  - `bro_exec`, `bro_resume`: accept optional `surface` from params.
  - Workflow / orchestration dispatch paths: surface from workflow spec or arc.
  - Default: `None` (preserves current behavior).

- Add `surface` field to `ExecParams` / `ResumeParams` (optional string).

- Extend `bro_mcp action=add` to accept optional `surface` field that appends
  `?surface=<id>` to the registered URL.

**Tests:**

- Dispatch with `surface="readonly"` → resolved filters include readonly
  disallow set + recursion guard.
- Dispatch without surface → identical to current behavior.
- Allow-intersection: global allow `[A, B, C]` + surface allow `[B, C, D]` →
  effective allow `[B, C]`.
- Disallow-additive: surface disallow `[X]` + brofile disallow `[Y]` → both
  denied.
- Claude, Codex, Copilot, Gemini filter args all reflect merged surface filters.
- Provider alias registration preserves query string in stored URL.

## Phase 4 — Debug tooling, docs, and example packet

**Goal:** Operational visibility. Packet authors can iterate on surface rules
without restarting providers.

**Scope:**

- Extend the Phase 1b replay tool with additional actions:

| Action | Purpose |
|--------|---------|
| `list` | List installed surface packets and their domains |
| `describe` | Show the effective rules for a given surface name |
| `replay` | Pure evaluation (from Phase 1b) |

- Add example packet at `examples/packets/mcp-surface-routing.json` with the
  three rules from the design doc (readonly, ops, default + catchall deny).

- Update `AGENTS.md` project section to mention MCP surfaces.

**Tests:**

- Example packet compiles via `bbox_compile` and audits cleanly.
- `describe` returns human-readable rule summary.

## Cross-cutting concerns

### Packet store changes

- No schema changes. Surface packets use domain `mcp-surface/routing`, stored via
  `bbox_compile`. New `load_latest_by_domain` method adds project-aware lookup.

### Tool universe

- Populated at daemon startup: `tool_universe: Vec<String>` on `SharedState`.
  Bare tool names extracted from `ToolRouter::list_all()`. Reused for pattern
  expansion in surface evaluation and dispatch filters.

### Error posture

| Condition | Behavior |
|-----------|----------|
| No surface packet installed | Passthrough (all tools visible) |
| Packet load error (corrupt) | Deny (fail closed) |
| No rule matches entity | Deny (fail closed) |
| Consequent parse error | Deny (fail closed) |
| Unknown surface name | Deny via catchall rule |
| Deny verdict on initialize | MCP error (not empty catalog) |

### Performance

- Surface evaluation on every `list_tools`/`call_tool`. Packet store read-locked
  briefly. Pattern expansion O(patterns × universe), both small (< 200 tools).
  No caching for v1.

### Tracing

- `tracing::debug!` on surface evaluation: surface name, verdict, matched rule
  id, visible tool count. `tracing::warn!` on deny verdicts and parse failures.

## Dependency graph

```
Phase 1  (verdict types, evaluator, project-aware lookup)
    │
    ├── Phase 1b (pure replay tool)
    │
    ├── Phase 2a (rmcp handler seam, hardcoded default)
    │       │
    │       └── Phase 2b (session binding from RequestContext)
    │               │
    │               └── Phase 3 (dispatch integration, allow-intersection)
    │                       │
    │                       └── Phase 4 (docs, example packet, describe)
    │
    └── Phase 1b can parallel Phase 2a
```

Phase 1 is prerequisite for everything. Phase 1b (replay) is cheap and should
land before 2b to validate packet semantics without rmcp plumbing. Phase 2a
validates the handler seam with zero session-binding risk. Phase 2b adds the
real URL binding. Phase 3 depends on 2b being stable. Phase 4 is polish.
