---
title: "System Memories \u2014 Runtime File Loading with Tera Templates"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
  - knowledge
---

# System Memories — Runtime File Loading with Tera Templates

Date: 2026-05-14
Status: design proposal v2 (converged through three-round review)

Related:
- `src/system_memory/mod.rs` — current compile-time implementation.
- `src/template.rs` — existing Tera infrastructure.
- `system-defaults/` — shipped defaults directory for brofiles, workflows, atoms, agents.
- `src/config.rs` — path resolution, `load_with()`, `resolve_paths()`.
- `src/tools/orchestrate.rs:20` — `system_memory::get()` caller using `sm.content` (owned `String` borrow after migration).
- `src/tools/knowledge.rs:186` — comment describing memories as "code-embedded" (update on migration).
- `src/tool_docs.rs` — tool descriptions with system-memory pointers (update on migration).

## Problem

System memories are 26 markdown runbooks embedded in the daemon binary via `include_str!` in `src/system_memory/mod.rs`. This creates three issues:

1. **Rebuild required for text changes.** Any wording tweak, tag addition, or new memory requires `cargo build` + daemon restart. This is a poor feedback loop for content that is purely textual.

2. **Thematic inconsistency.** Every other configurable surface — brofiles, packets, workflows, atoms, agents — lives in `system-defaults/` as user-editable files. System memories are the only compile-locked artifact in an otherwise DIY-customizable system.

3. **No user customization.** Operators cannot override or extend system memories per-deployment without forking and recompiling. They also cannot parameterize memory content through templating.

## Current State

**Structure:** A static `SYSTEM_MEMORIES` array of 26 `SystemMemory` entries (28 structs matching 26 `.md` files — two entries share no file), each with `id: &'static str`, `title: &'static str`, `tags: &'static [&'static str]`, and `content: &'static str` (loaded via `include_str!`).

**Files:** 26 `.md` files in `src/system_memory/` alongside `mod.rs`.

**Consumers:**
- `src/tools/knowledge.rs` — calls `system_memory::exact_query()`, `system_memory::search()`, `system_memory::format_for_listing()` for `bbox_knowledge` queries.
- `src/orchestration/atoms/validate.rs:1299` — reads `src/system_memory/refactor.md` directly from disk for atom validation tests.
- `src/tools/orchestrate.rs` — calls `system_memory::get("sm-workflow-orchestration")` for workflow runbook access.

**Search:** The `search()` function builds a `MemoryCorpus` with owned `String`/`Vec<String>` from the struct fields and runs weighted scoring (id > title > tags > content) against parsed query ASTs.

**Existing infrastructure:**
- `src/template.rs` — Tera `render()` and `render_file()`, already used for roadmap/workflow rendering.
- `system-defaults/` — directory with subdirs for atoms, brofiles, workflows, agents. Files are JSON artifacts loaded at runtime.
- `src/watcher.rs` — file watcher for `.bbox/` directories using the `notify` crate.
- Cargo.toml already depends on `tera` and `toml`.

## Design

### File Format: TOML Front Matter

Each memory is a single `.md` file with TOML front matter delimited by `+++`:

```markdown
+++
title = "Rule-packets — compile a reusable mechanism from examples"
tags = ["packets", "compile", "audit", "mechanism", "rubric"]
order = 3
template = false
+++

# Rule-packets — compile a reusable mechanism from examples

(markdown body)
```

**Why TOML front matter over alternatives:**

| Approach | Problem |
|----------|---------|
| Sidecar `.toml` | File sync drift — sidecar can be moved/renamed independently of content |
| Single manifest | Same problem, centralized — manifest entry can drift from the file it describes |
| YAML front matter (`---`) | Ambiguous with markdown tables and horizontal rules |
| **TOML front matter (`+++`)** | Single-file atomic, `toml` crate already in deps, matches project config language |

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `title` | string | yes | Human-readable title for listings |
| `tags` | string[] | yes | Searchable tags for query matching |
| `order` | uint | no | Listing priority (default 999); `agentic-opening-sequence` at order=0 |
| `template` | bool | no | Whether to render body through Tera (default false) |

**`id` is not stored in front matter.** It is derived from the filename stem: `rule-packets.md` → `sm-rule-packets`. This avoids a redundant field that could contradict the filename.

### File Naming

Filenames use the bare slug without `sm-` prefix:

- `system-defaults/memories/agentic-opening-sequence.md` → `sm-agentic-opening-sequence`
- `system-defaults/memories/refactor-rust.md` → `sm-refactor-rust`

The `sm-` prefix is a runtime ID convention, not a filesystem naming requirement.

### Template Rendering

**Opt-in via `template = true` in front matter.** Not marker sniffing.

`src/system_memory/refactor.md:401` contains literal `{{...}}` prose in its body. A heuristic like "render if body contains `{{`" would misfire on this content. The `template` front matter field is explicit and unambiguous.

When `template = true`, the body is rendered through `crate::template::render(&body, ctx)` before being stored in the catalog. When `template = false` (default), the body is stored verbatim. This fast-path skips Tera parsing entirely for plain markdown files.

**Template context** is a small, stable JSON object constructed once at daemon startup:

```json
{
  "version": "0.x.y",
  "mcp_name": "blackbox"
}
```

This is enough for parameterization (version strings in docs, tool name prefixes) without overcomplicating. The context can be extended later without breaking existing templates.

### MemoryCatalog Type

A pure, testable catalog type decoupled from global state:

```rust
#[derive(Debug, Clone)]
pub struct SystemMemory {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub content: String,  // post-Tera-rendered if template=true
}

pub struct MemoryCatalog {
    memories: Vec<SystemMemory>,
}

impl MemoryCatalog {
    pub fn load(defaults_dir: &Path, user_dir: Option<&Path>, ctx: &Value) -> Result<Self>;
    pub fn get(&self, id: &str) -> Option<&SystemMemory>;
    pub fn search(&self, query: Option<&str>) -> Vec<&SystemMemory>;
    pub fn format_for_listing(&self, m: &SystemMemory) -> String;
}
```

Production uses `OnceLock<MemoryCatalog>` as a thin global wrapper:

```rust
static SYSTEM_MEMORY_CATALOG: OnceLock<MemoryCatalog> = OnceLock::new();

pub fn init(defaults_dir: &Path, user_dir: Option<&Path>, ctx: &Value) -> Result<()> {
    SYSTEM_MEMORY_CATALOG
        .set(MemoryCatalog::load(defaults_dir, user_dir, ctx)?)
        .map_err(|_| anyhow!("system memory catalog already initialized"))
}
```

**Why `OnceLock` over alternatives:**

| Primitive | Problem |
|-----------|---------|
| `lazy_static!` | Not better than `OnceLock` for this case; adds a dependency |
| `RwLock` | Mutable but we don't need mutation after init; overhead for no benefit |
| `ArcSwap` | For hot-reload (future); v1 doesn't need it; return types would need to change to owned |
| **`OnceLock`** | Immutable after init, `'static` borrows work, stdlib, zero overhead |

Tests instantiate `MemoryCatalog` directly from temp directories or test fixtures. They never touch the global `OnceLock`. This avoids the "OnceLock can't reset" testability problem entirely.

### Overlay Resolution

Two layers for v1:

```
system-defaults/memories/          ← shipped defaults (read-only, version-controlled)
~/.config/<instance>/memories/     ← user overrides (last-write-wins)
```

**Layer semantics:**

- Files are keyed by filename stem → `sm-<stem>`.
- Files are sorted by filename before loading within each layer (deterministic traversal).
- Duplicate stems within a single layer are an error (fail closed at startup).
- User layer fully replaces default: both metadata and body are overwritten.
- Every override is logged at INFO level.
- Final catalog is sorted by `order` field, then by `id` for stable ordering.

**Config dir isolation** — the user overlay directory is derived from the selected config instance, not hardcoded:

```
memory overlay dir:
  BLACKBOX_MEMORY_DIR env var           ← highest priority
  [paths].memory_dir config field       ← if set
  <config_dir>/memories                 ← derived from config path
```

Where `<config_dir>` is the parent of the selected `config.toml`. This means:
- `blackbox.service` with default config → `~/.config/blackbox/memories/`
- `blackbox-dev.service` with `BLACKBOX_CONFIG=%h/.config/blackbox-dev/config.toml` → `~/.config/blackbox-dev/memories/`

The dev daemon never accidentally reads prod user overrides.

### Defaults Dir Resolution

For the shipped defaults directory:

```
defaults dir:
  BLACKBOX_DEFAULTS_DIR/memories        ← env var override
  [paths].defaults_dir/memories         ← config field
  <exe_dir>/../share/blackbox/memories  ← installed binary layout
  <cwd>/system-defaults/memories        ← dev fallback
```

This covers all deployment modes: installed binary with share tree, custom prefix via env/config, and bare `cargo run` development.

### Startup Behavior

System memory loading happens before `SharedState` construction, after config resolution.

**Failure modes:**

| Condition | Behavior |
|-----------|----------|
| Defaults dir missing | Fail closed with clear error message. Daemon cannot start without its runbooks — this is equivalent to a compile error in the current system. |
| User overlay dir missing | Silently ignored. Only defaults are loaded. |
| Malformed front matter | Fail closed with filename and parse error. |
| Empty `title` or `tags` | Fail closed. Every memory must have a title and at least one tag. |
| Duplicate stem within one layer | Fail closed. Likely a copy-paste error. |
| Tera render error (`template = true`) | Fail closed with filename and template error. |
| Missing file referenced by `validate.rs` | Test fails at compile time (path is in test code). |

### Catalog Discovery: `bbox_knowledge(category="system_memory")`

A new `category` filter value surfaces the system memory catalog through the existing `bbox_knowledge` tool.

**Why `system_memory` and not `memory`:** `Category::Memory` already exists in `src/knowledge.rs` as a runtime knowledge entry category. Reusing it would conflate two distinct concepts — durable user-authored knowledge entries vs. shipped daemon runbooks.

Accepted filter values (all map to the same behavior): `"system_memory"`, `"system-memory"`, `"system_memories"`.

**Output format:** A compact metadata-only listing, distinct from `format_for_listing()` which includes full memory bodies:

```
── System memories (26) ──────────────────────
[system] sm-agentic-opening-sequence — Agentic opening sequence — orient, search, inspect, traverse, answer
  tags: opening, grounding, first-step, ...
[system] sm-rule-packets — Rule-packets — compile a reusable mechanism from examples
  tags: packets, compile, audit, mechanism, ...
...
```

This is IDs + titles + tag previews. Full body is only included on direct `sm-<id>` queries (existing behavior).

### Direct File Reference: validate.rs

`src/orchestration/atoms/validate.rs:1299` reads `src/system_memory/refactor.md` directly from disk for atom validation tests. After the move, this path becomes:

```rust
let catalog = std::fs::read_to_string(
    crate_root.join("system-defaults/memories/refactor.md")
).expect("read sm-refactor");
```

The test must strip the front matter (`+++...+++` block) before asserting on content. A helper function like `strip_front_matter(s: &str) -> &str` can be shared between the loader and test code.

### Config Plumbing: Config Path into Path Resolution

Config-dir-derived user overlays need the active `config.toml` path inside `resolve_paths()`. Currently, `load_with()` computes `config_path` at `src/config.rs:599` but passes only `(&raw, &home)` to `resolve_paths()` at line 627 — the selected config path is discarded.

The fix: pass `config_path` into `resolve_paths()`:

```rust
// src/config.rs — load_with()
let paths = resolve_paths(&raw, &home, config_path.as_deref())?;

// src/config.rs — resolve_paths()
fn resolve_paths(raw: &RawConfig, home: &Path, config_path: Option<&Path>) -> Result<ResolvedPathConfig> {
    // ...
    let config_dir = config_path
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .or_else(|| home.join(".config").join("blackbox").into());
    // Use config_dir to derive user_memories_dir
}
```

This is a prerequisite for config-instance isolation. Without it, `<config_dir>/memories` cannot be resolved.

### Caller Audit: Stale Code-Facing Docs

After migration, several source files reference the old compile-time model:

- `src/tools/knowledge.rs:186` — comment describes memories as "code-embedded" and "baked into the binary". Update to describe file-based loading.
- `src/tool_docs.rs` — tool descriptions contain hints like "Full workflow in `sm-rule-packets` — query via bbox_knowledge." These remain correct (the API doesn't change) but any description implying compile-time embedding should be updated.
- `src/system_memory/mod.rs` module doc comment — describes the old lifecycle ("edit the `.md` file, rebuild the daemon, restart"). Rewrite for file-based lifecycle.

### Deployment Coverage

### Test Strategy

**Parser tests:** Table-driven tests for malformed front matter, CRLF, missing delimiters, duplicate IDs, invalid TOML, empty title/tags, ordering stability, and overlay replacement semantics. These use `MemoryCatalog::load()` against `tempfile::TempDir` directories — no global state involved.

**Shipped-content invariant tests:** Tests like `rule_packets_memory_embedded_and_nonempty`, `gap_notes_memory_embedded_and_teaches_envelope`, and `sm_refactor_uses_signposts_not_atom_ledger` are redirected to load from `system-defaults/memories/` rather than test fixtures. These validate real runbook invariants and must survive the move.

**Golden files:** Optional for `format_for_listing()` and the catalog metadata formatter, but not blocking for v1.

### Struct Migration

`SystemMemory` changes from `&'static` fields to owned types:

```rust
// Before
pub struct SystemMemory {
    pub id: &'static str,
    pub title: &'static str,
    pub tags: &'static [&'static str],
    pub content: &'static str,
}

// After
#[derive(Debug, Clone)]
pub struct SystemMemory {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub content: String,
}
```

Downstream function signatures (`get()`, `search()`, `exact_query()`) return `&SystemMemory` with `'static` lifetime via `OnceLock`'s guarantee. Callers that currently use `sm.content` where `content` was `&'static str` (e.g., `src/tools/orchestrate.rs:20`) automatically get `&String` via auto-deref — no caller changes needed. If a caller requires a `&str` explicitly, `.as_str()` or auto-coercion handles it.

### Deployment Coverage

Current deploy installs binaries and systemd units. The memory defaults need an explicit copy step added to all install instruction locations:

- `CLAUDE.md` — deploy section
- `README.md` — install snippets
- `PROJECT.md` — build & deploy section
- `docs/operations.md` — operational reference
- `docs/getting-started.md` — first-run setup
- `docs/operating-blackbox.md` — daemon management

New install step:

```bash
# Existing
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/bro ~/.local/bin/bro

# New
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

The `<exe_dir>/../share/blackbox/memories` resolution path makes this work for installed binaries (`~/.local/bin/blackboxd` → `~/.local/share/blackbox/memories`). Development uses `<cwd>/system-defaults/memories` directly. `blackbox.service` uses `~/.local/bin/blackboxd` and `blackbox-dev.service` uses `~/.local/bin/blackboxd-dev` — separate binaries for service isolation, but both live under `~/.local/bin/` so both resolve the same `~/.local/share/blackbox/memories` defaults tree. User overlays are config-instance-isolated per the overlay resolution section.

### Not In Scope (v1)

- **Hot reload / file watcher for memories.** Daemon restart is acceptable. The watcher infrastructure exists for `.bbox/` artifacts but is not needed here.
- **Project-local `.bbox/memories/`.** Would require per-project state in `SharedState`. User-local overlay is sufficient for v1.
- **Memory versioning or supersession.** Unlike packets/workflows, memories are not independently versioned artifacts. They ship with the daemon.
- **Memory authoring tooling.** No `bbox_memory_create` MCP tool. Users author `.md` files with front matter directly.

### Open Questions

None. The design was converged through two rounds of Codex review against the actual source.
