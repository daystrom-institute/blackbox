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

## Phase 1 - Surface table and visible-set computation

**Goal:** Introduce the surface configuration type and a pure visible-set
function. No changes to the MCP handler or HTTP stack.

**Scope:**

- Add `SurfaceConfig` in `crates/bbox-config/src/config.rs`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub disallow: Vec<String>,
}
```

- Add `visible_tool_set(surface: &SurfaceConfig, universe: &[String]) -> HashSet<String>`
  in `src/server/surface.rs`: normalize each pattern with
  `normalize_filter_pattern`, strip the blackbox MCP prefix, and match bare
  tool names with `glob_match`, disallow winning over a non-empty allow.

- Add `unknown_surface_message(surface)` so the wire head and dispatch paths
  report the same refusal text.

- The tool universe is the combined `bbox_tools + bro_tools` router catalog.
  Reuse it in both surface computation and dispatch filter expansion.

- Wire `mod surface;` into `src/server/mod.rs`.

**Tests (in `src/server/surface.rs`):**

- an empty surface shows the whole catalog.
- a non-empty allow hides non-matching tools.
- disallow wins over allow.
- canonical `mcp__blackbox__bbox_search`, dotted `mcp__blackbox__.bbox_search`,
  Copilot `blackbox(bbox_search)`, and bare `bbox_search` all match the same
  tool after normalization.

**Does not touch:** `BlackboxServer`, `ServerHandler`, HTTP routing, `StreamableHttpService`.

## Phase 1b - Built-in surfaces and config merge

**Goal:** Ship the built-in surface table and let daemon configuration
override or extend it.

**Scope:**

- Add `crates/bbox-config/src/default_surfaces.toml` with the `default`,
  `interactive`, `agent-internal`, `readonly` and `ops` surfaces, loaded by
  `default_surfaces()`.

- `Config::surfaces: BTreeMap<String, SurfaceConfig>` holds the built-in table
  merged with `[surfaces.<name>]` tables from the daemon config file: a config
  table replaces the built-in surface of the same name or adds a new one.

- Validate at config load: surface names are non-empty without surrounding
  whitespace, patterns are non-empty, unknown keys are rejected.

**Tests:**

- config tables override a built-in surface by name and add new surfaces;
  untouched built-ins survive the merge.
- empty patterns and unknown keys fail config load.
- the built-in agent-facing surfaces and `ops` partition the served catalog as
  intended.

## Phase 2a — rmcp handler seam with hardcoded surface

**Goal:** Override `list_tools`, `call_tool`, and `get_tool` in the existing
`#[tool_handler]` impl to enforce the session's visible set. Surface is
hardcoded to `"default"`, with no session binding yet.

**Scope:**

- Add `surface: OnceLock<Arc<str>>` and `surface_tools: OnceLock<Arc<HashSet<String>>>`
  to `BlackboxServer` in `src/server/state.rs` (both set during `initialize`):
  ```rust
  pub(crate) struct BlackboxServer {
      pub(crate) state: Arc<SharedState>,
      pub(crate) tool_router: ToolRouter<Self>,
      pub(crate) surface: OnceLock<Arc<str>>,
      pub(crate) surface_tools: OnceLock<Arc<HashSet<String>>>,
  }
  ```
  `surface` defaults to `"default"`. `surface_tools_for(name)` looks the name
  up in `config.surfaces` and returns `visible_tool_set` over the tool
  universe, or `None` for an unknown surface.

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
        // Look it up in config.surfaces; an unknown name fails initialize.
        // Store the name and its visible set (OnceLock, set once per session).
        // For 2a: hardcode "default" (no URI parsing yet).
        let Some(tools) = self.surface_tools_for("default") else {
            return Err(McpError::invalid_request(
                "tool surface denied: unknown MCP surface: default", None));
        };
        let _ = self.surface.set(Arc::from("default"));
        let _ = self.surface_tools.set(tools);
        // Delegate to default behavior
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if !self.session_tools().contains(name) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
    ) -> Result<ListToolsResult, McpError> {
        let visible = self.session_tools();
        let tools = self.tool_router.list_all().into_iter()
            .filter(|t| visible.contains(t.name.as_ref()))
            .collect();
        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if !self.session_tools().contains(request.name.as_ref()) {
            return Err(McpError::method_not_found(
                format!("tool not available on surface '{}': {}",
                    self.session_surface(), request.name)
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

- Integration test: `BlackboxServer` on the default surface → the `default`
  table's visible set.
- Integration test: a configured surface makes `list_tools` return the filtered
  set.
- Integration test: `call_tool` for hidden tool returns MCP error.
- Integration test: `get_tool` for hidden tool returns `None`.
- Integration test: an unknown surface fails `initialize` (not empty catalog).
- All existing `cargo test` passes (backward compatibility gate).

**Merge gate:** all existing tests pass.

## Phase 2b — Session binding from RequestContext

**Goal:** Read `?surface` from the `initialize` request URI via rmcp's
`RequestContext` extensions. Store on the session's `BlackboxServer`.

**Scope:**

- Implement `initialize` override to:
  1. Extract `http::request::Parts` from `context.extensions`.
  2. Parse `parts.uri.query()` for `surface` parameter.
  3. Default to `"default"` if absent.
  4. Look the surface up in `config.surfaces`. If it is missing, fail
     `initialize` with `tool surface denied: unknown MCP surface: <name>`.
  5. Set `self.surface` and `self.surface_tools` via `OnceLock`.

- `?project` is read separately into the session project context; it does
  not affect surface selection.

- Optionally add `SurfaceId` newtype for type-safe extraction from the URI
  query, but since we're reading from `http::request::Parts` directly (not axum),
  a simple string extraction is sufficient.

**Tests:**

- Initialize with `?surface=readonly` → session sees filtered `list_tools`.
- Initialize without `?surface` → `"default"` surface.
- Initialize with unknown surface → `initialize` returns MCP error.
- Subsequent requests on the same session ignore a different `?surface` in the
  URI (OnceLock already set).
- Two concurrent sessions with different surfaces each keep their init-time
  surface.

## Phase 3 — Dispatch integration and provider registration

**Goal:** Configured surfaces compose into `resolve_dispatch_filters` so spawned
bros inherit the correct tool boundary. Provider configs gain surface aliases.

**Scope:**

- Add `dispatch_surface_filters(&config.surfaces, surface: Option<&str>) -> Option<McpFilters>`:
  `None` when no surface is named or the surface is unrestricted, the
  surface's `allow`/`disallow` otherwise, and a deny-all filter
  (`disallow: ["*"]`) for an unknown surface.

- Extend `resolve_dispatch_filters` in `src/server/progress.rs` to accept `surface: Option<&str>`:
  - When present, merge the `dispatch_surface_filters` result into the
    effective filter set.
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
  - Brofile `surface` selectors fold into the child's filters the same way.
  - Default: `None` (preserves current behavior).

- Add `surface` field to `ExecParams` / `ResumeParams` (optional string).

- Extend `bro_mcp action=add` to accept optional `surface` field that appends
  `?surface=<id>` to the registered URL.

**Tests:**

- Dispatch with `surface="readonly"` → resolved filters include readonly
  disallow set + recursion guard.
- Dispatch without surface → identical to current behavior.
- Dispatch with an unknown surface → every tool denied.
- Allow-intersection: global allow `[A, B, C]` + surface allow `[B, C, D]` →
  effective allow `[B, C]`.
- Disallow-additive: surface disallow `[X]` + brofile disallow `[Y]` → both
  denied.
- Claude, Codex, Copilot filter args all reflect merged surface filters.
- Provider alias registration preserves query string in stored URL.

## Phase 4 - Docs and operator path

**Goal:** Operational visibility. Operators can read and change surfaces and
reach hidden tools.

**Scope:**

- Document the built-in surfaces, the `[surfaces.<name>]` config shape and the
  restart-to-apply rule in `docs/mcp-surfaces.md`.

- Operators reach tools hidden from agent-facing surfaces with
  `bro mcp call <tool> '<json>' --surface ops`.

- Update `AGENTS.md` project section to mention MCP surfaces.

**Tests:**

- An ops-only tool round-trips through `bro mcp call --surface ops`.
- `tools/list` on each built-in surface returns the configured visible set.

## Cross-cutting concerns

### Configuration

- Surfaces are part of daemon configuration: built-in defaults in
  `default_surfaces.toml` merged with `[surfaces.<name>]` tables by name. A
  change applies on daemon restart. There is no runtime install path, no
  surface versioning and no project-scoped surface.

### Tool universe

- Bare tool names extracted from the combined `ToolRouter::list_all()`. Reused
  for pattern expansion in surface computation and dispatch filters.

### Error posture

| Condition | Behavior |
|-----------|----------|
| Surface table with no patterns | Passthrough (all tools visible) |
| Invalid surface config (empty pattern, unknown key) | Config load fails |
| Unknown surface name on initialize | MCP error (not empty catalog) |
| Unknown surface name on dispatch | Deny-all child filter |

### Performance

- The visible set is computed once per session at `initialize`. Pattern
  matching is O(patterns × universe), both small. `list_tools`, `call_tool`
  and `get_tool` are set lookups.

## Dependency graph

```
Phase 1  (surface type, visible-set computation)
    │
    ├── Phase 1b (built-in table, config merge)
    │
    ├── Phase 2a (rmcp handler seam, hardcoded default)
    │       │
    │       └── Phase 2b (session binding from RequestContext)
    │               │
    │               └── Phase 3 (dispatch integration, allow-intersection)
    │                       │
    │                       └── Phase 4 (docs, operator path)
    │
    └── Phase 1b can parallel Phase 2a
```

Phase 1 is prerequisite for everything. Phase 1b (config merge) is cheap and
should land before 2b so the session binding reads the real surface table. Phase 2a
validates the handler seam with zero session-binding risk. Phase 2b adds the
real URL binding. Phase 3 depends on 2b being stable. Phase 4 is polish.
