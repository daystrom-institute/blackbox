# Rust Refactor Atoms — Batch 2

Date: 2026-05-13
Status: design proposal
Reviewers: DeepSeek V4 Pro (external review), operator review pending
Related: `design/archive/refactor-agents-impl.md`,
`design/proposed/refactor-rust-v2-invariants.md`,
`src/system_memory/refactor-rust.md`

## Problem

The Rust refactor surface has 7 atoms and 25+ primitive plan kinds. The Java
surface has 11 atoms against fewer plan kinds. Several high-frequency Rust
refactoring patterns require agents to hand-assemble multi-step sequences from
primitives — sequences that should be single-atoms. These atoms compose
*existing* primitives with zero new plan kinds.

A parallel set of atoms is blocked on missing plan kinds (filed as gap notes;
see "Blocked atoms" below). This doc covers only the atoms we can build today.

## Scope

### Ships in this batch (5 atoms, no new plan kinds)

| # | Atom | Cost | Primitive composition |
|---|------|------|----------------------|
| 1 | `rust-rename-symbol` | normal | `rust_lsp_rename` + `rust_public_api_guard` preflight + `cargo check` post-flight |
| 2 | `rust-extract-to-submodule` | normal | `extract_rust_items_to_submodule` + `cargo check --message-format=json` + `rust_compile_fix_round` + `cargo check` |
| 3 | `rust-organize-imports` | cheap | `rust_organize_imports` (LSP) alone; no `rust_minimize_imports` (redundant per review) |
| 4 | `rust-doc-harden` | cheap | `bbox_refactor_status` walk of pub items + analysis report; no new plan kind |
| 5 | `rust-cargo-add-dep` | cheap | `ensure_toml_table` for `[dependencies]` + structured TOML key insertion |

### Non-goals

- New plan kinds (see "Blocked atoms" and gap notes).
- Atoms that need LSP `textDocument/references` (`rust-find-usages`).
- Atoms that need RA `experimental/extractFunction` (`rust-extract-function`).
- Atoms that need new tree-sitter attribute insertion (`rust-add-feature-gate`).
- Atoms that need `super::` depth rebasing across directory trees
  (`rust-promote-module-to-crate`, `rust-restructure-crate`).
- Atoms that need type resolution for derive safety (`rust-audit-derivable`).

## Design

### Common patterns

All five atoms share the profile-atom shape from refactor-agents v1:

- `implementation.kind = "profile"`
- `implementation.brofile_ref = "brofile:rust-refactor-persona@v1"`
- `composition.may_invoke_atoms.kind = "none"`
- `effects.writes_files = true` (except `rust-doc-harden` plan-only mode)
- `effects.dispatches_runs = 1`, `effects.max_depth = 0`,
  `effects.uses_network = false`
- `supervision.oracle = "default"`, `supervision.advisor = "none"`

The atom prompt template follows the five-step protocol:
ground → plan → decide → apply → done-note.

### 1. `rust-rename-symbol` (normal)

**Description:** Project-wide Rust symbol rename with usage inventory,
public-API preflight, and post-flight validation.

**Inputs:**

```
required: project_dir, source_file, old_name, new_name
optional: apply (bool), validation_bin (string)
```

**Protocol:**

1. Ground: `bbox_refactor_status(file, project_dir)` to confirm `old_name`
   resolves in `source_file`. Copy exact name and kind.

2. Preflight: `bbox_refactor_plan(kind="rust_public_api_guard", source=source_file,
   toml_entries={"proposed_changes": [{"item": old_name, "action": "rename",
   "new_name": new_name}]})`. If severity is `"breaking"`, emit
   `bbox_note(kind="blocked")` with the report and return `status="blocked"`.
   If `"caution"`, surface in the plan but proceed (operator opted in by
   invoking the atom).

3. Apply: `bbox_refactor_run(confirm=true, steps=[`
   - `{"op":"plan","kind":"rust_lsp_rename","source":source_file,
      "item_names":[old_name],"new_text":new_name}`
   - `{"op":"command","command":"cargo","args":["check",
      "--message-format=json"],"capture":"rustc_json",
      "on_failure":"continue_for_repair"}`
   - `{"op":"plan","kind":"rust_compile_fix_round"}`
   - `{"op":"command","command":"cargo","args":["check"],"required":true}`
   - `{"op":"command","command":"cargo","args":["test","--bin",
      validation_bin],"required":true}`
   `])`

4. Done: `bbox_note(kind="done", body=<rename applied, api_guard severity,
   files_touched count, cargo result>)`.

**Anti-patterns:**

- Do not use for literal text replacement — use `replace_text` for that.
- Do not rename items in `std`/`core`/external crate surfaces.
- Do not skip the `rust_public_api_guard` preflight — it catches
  cross-crate breaks that `rust_lsp_rename` silently commits.

**When to use:**

- Renaming a Rust struct, enum, trait, function, method, module, constant,
  or type alias across a project.
- Preflight before a large rename to assess blast radius.

---

### 2. `rust-extract-to-submodule` (normal)

**Description:** Extract named top-level Rust items into a new child submodule
file with automatic module declaration, visibility widening, and import
re-export in the parent.

**Inputs:**

```
required: project_dir, source_file, target_file, item_names
optional: item_kinds (array), visibility (string, default "pub(super)"),
          module_name (string, default target_file stem),
          target_prelude (string, default "use super::*;"),
          apply (bool), validation_bin (string)
```

**Protocol:**

1. Ground: `bbox_refactor_status(file=source_file, project_dir)` to confirm
   every name in `item_names` resolves. Copy exact kinds.

2. Plan: `bbox_refactor_plan(kind="extract_rust_items_to_submodule",
   source=source_file, target=target_file, item_names, item_kinds,
   visibility, module_name, target_prelude, project_dir)`. Inspect the plan
   for items selected, visibility transforms, and use-decl contents.

3. Decide: if the plan reports `leftovers` that reference moved items
   (meaning the parent still needs them but they weren't re-exported), surface
   as a caution. Proceed unless blocked.

4. Apply: `bbox_refactor_run(confirm=true, steps=[`
   - `{"op":"plan","kind":"extract_rust_items_to_submodule",...}`
   - `{"op":"command","command":"cargo","args":["check",
      "--message-format=json"],"capture":"rustc_json",
      "on_failure":"continue_for_repair"}`
   - `{"op":"plan","kind":"rust_compile_fix_round"}`
   - `{"op":"command","command":"cargo","args":["check"],"required":true}`
   - `{"op":"command","command":"cargo","args":["test","--bin",
      validation_bin],"required":true}`
   `])`

5. Done: `bbox_note(kind="done", body=<items moved, target_file, visibility,
   compile_fix_round edits, cargo result>)`.

**Anti-patterns:**

- Do not use for `impl_method` items — use `split_rust_impl_methods_to_submodule`
  or the `rust-split-god-impl` atom instead.
- Do not use when you want different visibility per moved item — fall back to
  the 5-step primitive sequence.
- Do not use on non-Rust files.

**When to use:**

- A source file has a cluster of related top-level items (structs, enums,
  functions, constants) that belong in their own module.
- Preparing a file for further per-domain extraction.
- Incremental decomposition of a growing module.

---

### 3. `rust-organize-imports` (cheap)

**Description:** Clean up imports in one Rust file via rust-analyzer's
`source.organizeImports` code action.

**Inputs:**

```
required: project_dir, source_file
optional: apply (bool)
```

**Protocol:**

1. Ground: confirm `source_file` exists and parses as Rust via
   `bbox_refactor_status`.

2. Plan: `bbox_refactor_plan(kind="rust_organize_imports",
   source=source_file, project_dir)`.

3. Apply (if `apply=true`): `bbox_refactor_apply(confirm=true)`.

4. Done: `bbox_note(kind="done", body=<file, edits count>)`.

**Anti-patterns:**

- Do not chain `rust_minimize_imports` after this — LSP organize already
  handles unused-import removal. Running both is redundant and risks conflict.
- Do not use when rust-analyzer is unavailable — the plan kind returns
  `error.lsp_unavailable` cleanly.
- Do not use on non-Rust files.

**When to use:**

- After a refactoring step that left stale or unordered imports.
- Before committing to catch unused imports.
- As a cleanup pass inside a compound run after extraction.

---

### 4. `rust-doc-harden` (cheap)

**Description:** Audit undocumented `pub` and `pub(crate)` items in a Rust
file and return a structured report. Optionally insert doc-stub comments.

**Inputs:**

```
required: project_dir, source_file
optional: apply (bool, default false — analysis-only),
          scope (string: "pub" | "pub_and_pub_crate", default "pub"),
          allow_list (array of item names to skip),
          stub_style (string: "todo" | "allow", default "todo")
```

**Protocol:**

1. Ground: `bbox_refactor_status(file=source_file, project_dir,
   include_attributes=true)` to get all items with their attributes.

2. Analyze: walk returned items. For each with visibility containing `pub`
   (or `pub(crate)` when `scope="pub_and_pub_crate"`):
   - Check if the item has a `///` or `//!` doc comment attribute.
   - If missing and not in `allow_list`, add to `undocumented` report.

3. If `apply=false` (default): return the `undocumented` list as analysis.

4. If `apply=true`: for each undocumented item, emit a `replace_text` step
   that inserts `/// TODO: document` above the item (when `stub_style="todo"`,
   the default). Apply via `bbox_refactor_run`.

5. Done: `bbox_note(kind="done", body=<total_pub_items, undocumented count,
   stubs_inserted count>)`.

**Note:** This atom does NOT need a new plan kind. It composes
`bbox_refactor_status` (which returns items with attribute info) with
`replace_text` for stub insertion. The analysis pass is pure computation
over the status response.

**Anti-patterns:**

- Do not use as a CI gate — use `#![warn(missing_docs)]` for that.
- Do not overwrite existing doc comments.
- Do not use `stub_style="allow"` without operator intent — `#[allow(missing_docs)]`
  permanently hides the item from future audits. The `todo` default creates
  visible, grep-able debt instead of suppressing it.
- Do not use on non-Rust files.

**When to use:**

- Before enabling `#![warn(missing_docs)]` on a crate — assess scope first.
- After a batch of agent-authored pub items that likely lack docs.
- As a pre-commit hygiene check.

---

### 5. `rust-cargo-add-dep` (cheap)

**Description:** Add a dependency entry to a Rust project's `Cargo.toml`
with structured TOML editing.

**Inputs:**

```
required: project_dir, crate_name, version
optional: features (array of feature flags),
          optional (bool, default false),
          git (string, git URL override),
          branch (string, git branch),
          path (string, local path override),
          toml_table (string: "dependencies" | "dev-dependencies" |
                      "build-dependencies", default "dependencies"),
          apply (bool)
```

**Protocol:**

1. Ground: read `Cargo.toml` at `project_dir/Cargo.toml`. Parse existing
   `[dependencies]` table. Reject if `crate_name` already present (idempotent
   guard — operator should use `bbox_refactor_plan(kind="ensure_toml_table")`
   to update instead).

2. Plan: `bbox_refactor_plan(kind="ensure_toml_table",
   source="Cargo.toml", project_dir,
   toml_table=<toml_table>,
   toml_entries={<crate_name>: <value>})` where `<value>` is:
   - Just `version` string for simple deps
   - `{ version = version, features = [...], optional = true }` for complex deps
   - `{ git = url, branch = branch }` for git deps
   - `{ path = path }` for local deps

3. Apply: `bbox_refactor_apply(confirm=true)`.

4. Validate: `{"op":"command","command":"cargo","args":["check"],
   "required":true}` confirms the dependency resolves.

5. Done: `bbox_note(kind="done", body=<crate_name, version, cargo check result>)`.

**Anti-patterns:**

- Do not use to update an existing dependency — reject with clear message.
- Do not use on non-Rust projects.

**When to use:**

- Adding a new crate dependency to `Cargo.toml`.
- After an agent identifies a needed import that isn't in the dependency set.
- Safer than hand-editing TOML — prevents syntax errors and duplicates.

---

## Blocked atoms (deferred, gap notes filed)

These atoms need plan kinds or toolbelt augmentations that don't exist yet.
Gap notes are filed as `bbox_note(kind="followup")` with
`blackbox.gap_note.v1` bodies.

| Atom | Blocker | Gap note ID |
|------|---------|-------------|
| `rust-find-usages` | `rust_find_references` plan kind (LSP `textDocument/references`) | note-ca7d7b7d |
| `rust-extract-function` | `rust_ra_extract_function` plan kind (RA `experimental/extractFunction`) | note-24bde1d7 |
| `rust-audit-derivable` | `rust_derive_audit` plan kind (type resolution) | note-1fae72a5 |
| `rust-derive-from-fields` | Same as above (split into audit + apply) | note-1fae72a5 |
| `rust-add-feature-gate` | `rust_add_cfg_attribute` + `rust_add_feature_to_cargo` plan kinds | note-4e4e7382 |
| `rust-conditional-compile` | Same `rust_add_cfg_attribute` plan kind | note-4e4e7382 |
| `rust-promote-module-to-crate` | `rust_restructure_module_tree` plan kind (directory tree + `super::` rebasing) | note-f16cee20 |
| `rust-restructure-crate` | Same as above | note-f16cee20 |
| `rust-wrap-in-result` | `rust_wrap_return_in_result` plan kind (return-expression wrapping) | note-f8dda062 |
| `rust-inline-module` | `rust_inline_module` plan kind (reverse of extract-to-submodule) | note-9d2b4184 |
| `rust-split-trait` | `rust_split_trait` plan kind (supertrait wiring) | new gap note needed |
| `rust-newtype-wrap` | `rust_generate_derives` plan kind (Deref/AsRef boilerplate) | new gap note needed |
| `rust-enum-from-match` | `rust_match_to_enum` + `rust_generate_from_impl` plan kinds | new gap note needed |
| `rust-migrate-mods-to-lib` | Not a plan_dispatch primitive; wraps a macro expansion | note-677e0430 |

## Implementation notes

### File locations

Atom manifests go in `examples/agents/rust-refactor/` following the existing
convention (see `examples/agents/` for the Java refactor agent manifests).
Each atom is a single JSON file with the manifest schema from
`src/orchestration/atoms/types.rs`.

### Registration

Atoms install via `bbox_artifact_install(kind="atom", source=<path>)`.
The daemon picks them up on next restart or hot-reload.

### Validation

Each atom's prompt template references only plan kinds that exist in
`plan_dispatch()` at `src/refactor/mod.rs:1110-1178`. The atom validation
suite (`src/orchestration/atoms/validate.rs`) will confirm this on install.

### Cost class rationale

- **cheap**: single plan call, no compile cycle, analysis-only or trivial
  TOML edit (`rust-organize-imports`, `rust-doc-harden`, `rust-cargo-add-dep`).
- **normal**: compound run with compile-fix loop and test validation
  (`rust-rename-symbol`, `rust-extract-to-submodule`).

### DeepSeek review corrections applied

1. `rust-find-usages` composition was wrong — `rust_ra_classify_callbacks`
   is NOT a general usage finder. Removed from this batch; blocked on
   `rust_find_references`.
2. `rust-organize-imports` was revised to drop `rust_minimize_imports` —
   LSP organize already handles unused imports; running both is redundant.
3. `rust-migrate-mods-to-lib` removed from this batch —
   `migrate_rust_mods_to_lib` is a `bbox_refactor_run` macro expansion, not
   a `plan_dispatch` primitive. Needs a separate design for wrapping the
   macro.
4. `rust-derive-from-fields` split into audit-only (blocked) + apply-without-
   auto-delete. Removed from this batch pending `rust_derive_audit`.

## Resolved design decisions

- **`rust-doc-harden` stub style:** Default `/// TODO` stubs, not
  `#[allow(missing_docs)]`. Stubs are grep-able, show up in `cargo doc`,
  and create accountable debt. `#[allow]` permanently hides items from
  future audits. The `stub_style` input lets operators opt into `allow`
  when they have a genuine reason to suppress.

- **`rust-cargo-add-dep` table scope:** Support `"dependencies"`,
  `"dev-dependencies"`, and `"build-dependencies"` from the start via a
  `toml_table` input parameter. `ensure_toml_table` already accepts
  arbitrary table names — the marginal cost is zero.

- **`rust-rename-symbol` Cargo.toml boundary:** Not in scope. `rust_lsp_rename`
  operates on Rust source symbols only — it never touches `Cargo.toml`. Path
  references in `[dependencies]` only matter when renaming the crate itself
  (package name, lib name, workspace members), which is a fundamentally
  different operation. That would be a separate `rust-rename-crate` atom if
  ever needed.
