# System Memories Runtime Loading — Implementation Plan

Date: 2026-05-14
Status: draft implementation plan
Companion to: [System Memories Runtime Loading](system-memories-runtime-loading.md)

Related:
- [System Events Impl](system-events-impl.md) — phase DAG pattern this doc follows.
- [Atom System](../orchestration/atoms/atom-system.md) — `system-defaults/atoms/` precedent for shipped file artifacts.
- `src/template.rs` — Tera infrastructure.
- `src/config.rs` — path resolution and config instance isolation.

## Implementation Thesis

Build the loader before the features.

The tempting path is to move files, add Tera, add overlay, and add discovery all at once. That conflates the mechanical migration with three independent capability additions. The load-bearing work is the catalog type and front matter parser — once `MemoryCatalog::load()` works end-to-end against real files, everything else stacks on top without risk of regressions in the core loading path.

Core build order:

```text
loader + catalog type
  -> file migration + startup wiring
  -> test migration
  -> Tera rendering
  -> user overlay + config isolation
  -> discovery tooling
  -> deploy step
```

## Phase DAG

```text
Phase 0 ─▶ Phase 1 ─▶ Phase 2 ─▶ Phase 3 ─▶ Phase 4 ─▶ Phase 5
              │           │
              └── tests ──┘
```

Phase 0 creates the loader and catalog type with tests against fixtures. Phase 1 migrates files, wires startup, and migrates all tests (file migration and test migration are one atomic phase — the old `SYSTEM_MEMORIES` constant and its tests are removed and replaced in the same commit). Phase 2 adds Tera rendering. Phase 3 adds user overlay with config isolation. Phase 4 adds discovery tooling. Phase 5 adds the deploy step.

Each phase is independently shippable — the daemon works correctly after any single phase.

---

## Phase 0: Loader + Catalog Type

Create the loading infrastructure without touching existing code.

### New files

**`src/system_memory/loader.rs`** — front matter parser and directory loader.

- `MemoryFrontMatter` struct: `title: String`, `tags: Vec<String>`, `order: usize` (default 999), `template: bool` (default false). Deserialized via `toml` crate.
- `RawMemory` struct: `slug: String`, `front_matter: MemoryFrontMatter`, `body: String`.
- `parse_memory_file(slug, content) -> Result<RawMemory>` — validates `+++` delimiters, parses TOML, strips front matter, returns body. Rejects: missing delimiters, invalid TOML, empty title, empty tags.
- `load_dir(dir: &Path) -> Result<Vec<RawMemory>>` — reads all `.md` files from a directory, parses each, sorts by `order`. Returns empty vec for missing directory.

**`src/system_memory/catalog.rs`** — the `MemoryCatalog` type.

- `SystemMemory` struct with owned fields: `id: String`, `title: String`, `tags: Vec<String>`, `content: String`.
- `MemoryCatalog` struct wrapping `Vec<SystemMemory>`.
- `MemoryCatalog::load(defaults_dir, user_dir: Option<&Path>, ctx: &Value) -> Result<Self>` — calls `loader::load_dir` for defaults then user overlay, constructs `sm-<slug>` IDs, detects duplicate stems within a layer (error), logs overrides at INFO, sorts by order then id.
- `MemoryCatalog::get(&self, id: &str) -> Option<&SystemMemory>` — accepts `sm-<slug>` or bare `<slug>`.
- `MemoryCatalog::search(&self, query: Option<&str>) -> Vec<&SystemMemory>` — ported from current `search()` logic: weighted scoring across id/title/tags/content, query AST parsing.
- `MemoryCatalog::format_for_listing(&self, m: &SystemMemory) -> String` — ported from current `format_for_listing()`.
- `MemoryCatalog::format_catalog_summary(&self, query: Option<&str>) -> String` — compact metadata-only listing for `category="system_memory"` discovery. Accepts optional query filter to narrow results (uses same search logic as `search()` but returns summary format instead of full bodies).

### Test fixtures

**`src/system_memory/testdata/`** — 3 minimal memory files for unit tests:

- `alpha.md` — `order = 0`, minimal tags, no template flag.
- `beta.md` — `order = 1`, multiple tags.
- `gamma.md` — `order = 2`, `template = true`, body with `{{ version }}` placeholder.

Plus a `malformed/` subdirectory with error-case fixtures: missing delimiter, invalid TOML, empty title, duplicate slug.

### Tests in Phase 0

- `parse_memory_file` tests: valid front matter, missing opening delimiter, missing closing delimiter, invalid TOML, empty title, empty tags, CRLF line endings.
- `load_dir` tests: happy path with testdata, missing directory returns empty, malformed file returns error.
- `MemoryCatalog::load` tests: defaults-only, defaults + user overlay, user override replaces default, duplicate slug within one layer errors, ordering stability.
- `MemoryCatalog::get` tests: canonical `sm-<slug>`, bare `<slug>`, nonexistent.
- `MemoryCatalog::search` tests: port existing search tests against testdata (by tag, by title, by content, AND/OR/NOT, case insensitive, empty returns all).
- `MemoryCatalog::format_for_listing` tests: golden output check.
- `MemoryCatalog::format_catalog_summary` tests: metadata-only, no full bodies.

### Does not touch

- `src/system_memory/mod.rs` — unchanged. Existing `SYSTEM_MEMORIES` constant and `include_str!` array remain the active code path.
- `src/main.rs` — no startup wiring yet.
- No `.md` files move.

---

## Phase 1: File Migration + Startup Wiring

Move files and switch the active code path.

### File migration

For each of the 26 `.md` files in `src/system_memory/`:

1. Move to `system-defaults/memories/<slug>.md` (drop `sm-` prefix).
2. Prepend TOML front matter constructed from the current `SYSTEM_MEMORIES` array entry: `title`, `tags`, `order` (from position index), `template = false`.
3. Verify body content is byte-identical to the original `include_str!` source (no content changes).

`order` values assigned from current array position: `agentic-opening-sequence` = 0, `atoms` = 1, `rule-packets` = 2, ..., through to `whiteboards` = 25.

### Update `src/system_memory/mod.rs`

Replace the module body:

- Remove the `SYSTEM_MEMORIES` constant and all 28 `SystemMemory` entries with `include_str!`.
- Remove `get()`, `exact_query()`, `search()`, `format_for_listing()` free functions.
- Add `mod catalog; mod loader;` and re-export `catalog::MemoryCatalog` and `catalog::SystemMemory`.
- Add `static SYSTEM_MEMORY_CATALOG: OnceLock<MemoryCatalog> = OnceLock::new();`.
- Add `pub fn init(defaults_dir, user_dir, ctx) -> Result<()>` — calls `MemoryCatalog::load` and sets the OnceLock.
- Add thin wrappers `get()`, `exact_query()`, `search()` that delegate to `SYSTEM_MEMORY_CATALOG.get().unwrap()`.
- `format_for_listing` becomes a direct re-export or free function delegating to the catalog.

Public API remains identical: `system_memory::get()`, `system_memory::search()`, `system_memory::exact_query()`, `system_memory::format_for_listing()`.

### Wire startup in `src/main.rs`

After config load (~line 265), before `SharedState` construction (~line 506):

```rust
let defaults_memories_dir = resolve_defaults_memories_dir(&cfg);
let user_memories_dir = resolve_user_memories_dir(&cfg);
let memory_ctx = serde_json::json!({
    "version": env!("CARGO_PKG_VERSION"),
    "mcp_name": &cfg.daemon.mcp_name,
});
system_memory::init(&defaults_memories_dir, user_memories_dir.as_deref(), &memory_ctx)?;
```

### Path resolution helpers

Add two resolution functions (in `src/config.rs` or inline in `main.rs`):

`resolve_defaults_memories_dir(cfg)`:
1. `BLACKBOX_DEFAULTS_DIR` env var + `/memories`
2. `[paths].defaults_dir` config field + `/memories` (new field in `RawPathsConfig`)
3. `<current_exe_dir>/../share/blackbox/memories`
4. `<CARGO_MANIFEST_DIR>/system-defaults/memories` (dev fallback via compile-time env)

`resolve_user_memories_dir(cfg)`:
1. `BLACKBOX_MEMORY_DIR` env var
2. `[paths].memory_dir` config field (new field in `RawPathsConfig`)
3. `<config_dir>/memories` where `config_dir` = parent of active `config.toml`
4. `None` if config dir doesn't resolve (no user overlay)

### Config additions in `src/config.rs`

Add to `RawPathsConfig`:

```rust
pub defaults_dir: Option<PathBuf>,
pub memory_dir: Option<PathBuf>,
```

Add to `ResolvedPathConfig`:

```rust
pub defaults_memories_dir: PathBuf,
pub user_memories_dir: Option<PathBuf>,
```

**Config plumbing prerequisite:** `resolve_paths()` currently receives `(&raw, &home)` at `src/config.rs:627` but needs the selected `config_path` to derive the user overlay directory. Change the signature to `resolve_paths(&raw, &home, config_path: Option<&Path>)` and pass `config_path.as_deref()` from `load_with()` at line 627. Inside `resolve_paths`, derive `config_dir` from `config_path.parent()` for the user memories dir.

### Defaults fail-closed behavior

`MemoryCatalog::load()` must explicitly error if the defaults directory is missing or empty after loading. `loader::load_dir()` returns an empty vec for missing directories (correct for user overlay), but `MemoryCatalog::load()` wraps the call:

```rust
let defaults = load_dir(defaults_dir)?;
if defaults.is_empty() {
    anyhow::bail!("system memory defaults directory is empty or missing: {}", defaults_dir.display());
}
```

This preserves the fail-closed invariant from the design doc.

### Update direct file reference

`src/orchestration/atoms/validate.rs:1299` — change path from `src/system_memory/refactor.md` to `system-defaults/memories/refactor.md`. The test reads the raw file and must skip front matter — update the assertion to account for the `+++` header block.

### Update stale code-facing docs

- `src/tools/knowledge.rs:186` — comment describing memories as "code-embedded" and "baked into the binary". Rewrite for file-based loading.
- `src/system_memory/mod.rs` module doc comment — describes the old lifecycle. Rewrite for file-based lifecycle.
- `src/tool_docs.rs` — scan for any descriptions implying compile-time embedding and update.

### Test migration (same commit as file migration)

The old tests in `mod.rs` reference `SYSTEM_MEMORIES` directly (e.g., `src/system_memory/mod.rs:1019`, `mod.rs:1041`). These are removed and replaced in the same commit as the file migration:

**Shipped-content invariant tests** — Tests that assert specific memory content (`rule_packets_memory_embedded_and_nonempty`, `gap_notes_memory_embedded_and_teaches_envelope`, `search_finds_refactor_language_memories`) load the real `system-defaults/memories/` files via a test helper:

```rust
#[cfg(test)]
fn test_catalog() -> MemoryCatalog {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("system-defaults/memories");
    MemoryCatalog::load(&dir, None, &serde_json::json!({})).unwrap()
}
```

**Port search tests** — All 20+ tests ported to call `test_catalog()` instead of referencing `SYSTEM_MEMORIES`:

- `search_finds_by_tag_query`, `search_finds_by_id_query`, `search_exact_canonical_id_does_not_expand_prefix_family`, `search_bare_slug_still_behaves_as_search_term`, `search_finds_by_title_query`, `search_finds_by_body_content`, `search_case_insensitive`, `search_defaults_adjacent_terms_to_or`, `search_honors_and_and_exclusion`, `search_empty_returns_all`, `get_accepts_canonical_and_bare`, `format_for_listing_has_system_prefix`, `no_duplicate_ids`, `all_ids_canonical_prefix`.
- Content-specific tests for gap-notes, refactor language memories, and `gap_notes_memory_is_distinct_from_side_channel_notes`.

**Parser round-trip test** — Verifies all 26 shipped memory files parse cleanly through `loader::parse_memory_file()` with valid front matter.

### Delete

- All 26 `.md` files in `src/system_memory/` (moved to `system-defaults/memories/`).
- The old `SYSTEM_MEMORIES` constant and all `include_str!` calls in `mod.rs`.
- The old test module in `mod.rs` (replaced by tests in `catalog.rs` and `loader.rs`).

### Verification

- `cargo build --release` compiles.
- `cargo test --bin blackboxd` — all tests pass (migrated to MemoryCatalog-based tests).
- Start daemon, `bbox_knowledge(query="sm-rule-packets")` returns the full runbook.
- `bbox_knowledge(query="refactor")` returns the refactor catalog plus language variants.

---

## Phase 2: Tera Rendering

Enable template rendering for memories that opt in.

### Changes

In `MemoryCatalog::load()`, after parsing each file:

```rust
let content = if raw.front_matter.template {
    crate::template::render(&raw.body, ctx)
        .with_context(|| format!("rendering template for {}", raw.slug))?
} else {
    raw.body
};
```

No other code changes. The template context is already passed to `load()` from Phase 1.

### Test additions

- Add a test fixture with `template = true` and `{{ version }}` in body. Verify the rendered content contains the actual version string.
- Add a test fixture with `template = true` and invalid Tera syntax. Verify it returns an error.
- Verify `refactor.md` (which has literal `{{...}}` prose) with `template = false` is stored verbatim — no false-positive rendering.

### Verification

- All existing tests still pass (no shipped memory has `template = true` yet).
- New template tests pass.

---

## Phase 3: User Overlay + Config Isolation

Enable per-instance user overrides.

### Changes

Already wired in Phase 1 (`user_dir: Option<&Path>` parameter in `init()`). This phase validates and hardens the overlay behavior.

### Overlay semantics tests

Using `tempfile::TempDir`:

- User file replaces default file (same slug, different title/body).
- User file adds a new memory not in defaults.
- Removing user file reveals the default again (test by loading defaults-only vs defaults+user).
- Two user files with same slug within user layer → error.
- Empty user dir → defaults load normally.

### Config isolation verification

- Verify `resolve_user_memories_dir` returns `~/.config/blackbox/memories` for default config.
- Verify it returns `~/.config/blackbox-dev/memories` when `BLACKBOX_CONFIG` points to blackbox-dev config.
- Verify `BLACKBOX_MEMORY_DIR` env var takes precedence over config-derived path.

### Verification

- All overlay tests pass.
- Daemon starts cleanly with no user dir (defaults only).
- Daemon starts cleanly with user dir containing an override.

---

## Phase 4: Discovery Tooling

Add `category="system_memory"` filter to `bbox_knowledge`.

### Changes in `src/tools/knowledge.rs`

Add a check before the main knowledge query path:

```rust
if matches_system_memory_catalog(p.category.as_deref()) {
    let catalog = system_memory::catalog();
    return Self::ok_text(&catalog.format_catalog_summary(p.query.as_deref()));
}
```

Where `matches_system_memory_catalog` accepts `"system_memory"`, `"system-memory"`, `"system_memories"`.

This returns metadata-only (IDs, titles, tag previews). Full body is only on direct `sm-<id>` queries (existing behavior).

### Tests

- `bbox_knowledge(category="system_memory")` returns all 26 memories with IDs and titles.
- `bbox_knowledge(category="system_memory", query="refactor")` still works — query filters the catalog listing.
- `bbox_knowledge(category="memory")` does NOT trigger system memory catalog — it queries runtime knowledge entries as before.

### Verification

- MCP call with `category="system_memory"` lists all memories compactly.
- Direct `query="sm-refactor"` still returns full body (unchanged behavior).

---

## Phase 5: Deploy Step

Add memory defaults to the install target.

### Changes to deploy instructions

Update all install instruction locations:

- `CLAUDE.md` — deploy section
- `README.md` — install snippets
- `PROJECT.md` — build & deploy section
- `docs/operations.md` — operational reference
- `docs/getting-started.md` — first-run setup
- `docs/operating-blackbox.md` — daemon management

Add to each:

```bash
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

### Systemd unit changes

No changes needed — `<exe_dir>/../share/blackbox/memories` resolves correctly for `~/.local/bin/blackboxd` → `~/.local/share/blackbox/memories`.

### Update `system-defaults/README.md`

Add `memories/` row to the directory layout table documenting the new directory and its file format.

### Verification

- Install from clean build. Verify `~/.local/share/blackbox/memories/` contains all 26 files.
- Start installed `blackbox.service`. Verify memories load from `~/.local/share/blackbox/memories/`.
- Start installed `blackbox-dev.service` (with dev config). Verify memories load from same share tree, but user overlay resolves to `~/.config/blackbox-dev/memories/`.
- Start dev daemon (via `cargo run`). Verify memories load from `<cwd>/system-defaults/memories/`.
- `cargo test --bin blackboxd` — all tests pass from installed layout.
