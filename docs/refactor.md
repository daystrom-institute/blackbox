# Refactor Mechanization

Tree-sitter-backed structural refactoring: syntax inventory, dry-run
plans, hash-checked apply, transactional compound runs. The toolchain
knows where code is; language servers and compilers know what it means.
Use these tools for the mechanical half, LSP and `cargo check`/`javac`
for the semantic half.

Full runbook (plan kinds, language caveats, validation commands):
`bbox_knowledge(query="sm-refactor")`.

## Tool overview

| Tool | When |
|---|---|
| `bbox_code_symbols` | Find method/function/type line ranges across a project |
| `bbox_code_query` | Run a tree-sitter S-expression query against one file |
| `bbox_code_node_describe` | Discover grammar shape at a line/column before writing a query |
| `bbox_refactor_status` | Inventory refactorable items in a file, confirm parse health |
| `bbox_refactor_project_refs` | Ground `project_file:` entity refs for a source file |
| `bbox_refactor_plan` | Create a dry-run structural refactor plan |
| `bbox_refactor_apply` | Apply a reviewed plan with hash checks + rollback |
| `bbox_refactor_run` | Transactional compound run: multiple plans + validation commands |

## Workflow

```
1. Inventory   bbox_code_symbols / bbox_refactor_status
2. Plan        bbox_refactor_plan (dry run, reviewable JSON)
3. Review      read the plan — check hashes, edits, leftovers
4. Apply       bbox_refactor_apply(confirm=true)
5. Validate    language-specific check (cargo check, javac, etc.)
```

For multi-step restructuring: `bbox_refactor_run` instead of steps 2–5.

## Inventory tools

### `bbox_code_symbols`

Project-wide symbol search with kind filtering. Returns file, language,
kind, name, and line range. Use this instead of `rg -n` when you need
exact line numbers for method/function/type names.

```
bbox_code_symbols(
  project_dir="/repo/x",
  query="readFromProperties",
  languages=["java"],
  item_kinds=["method_declaration"],
  limit=20
)
```

### `bbox_code_query`

Tree-sitter S-expression query against a single file. Returns matched
nodes, byte/line ranges, and optional text. Use after you know the file;
use `bbox_code_symbols` first when you don't.

```
bbox_code_query(
  file="src/foo.rs",
  query="(function_item name: (identifier) @name)"
)
```

### `bbox_code_node_describe`

Discover the grammar shape at a position before writing a query. Returns
node kind, parent chain, named children, and sibling summaries.

```
bbox_code_node_describe(file="src/foo.rs", line=42, column=12)
```

### `bbox_refactor_status`

Inventory a file for refactorable items and confirm tree-sitter parse
health. Use before planning when you need exact item names/kinds to pass
into a plan.

```
bbox_refactor_status(
  file="src/path/to/File.java",
  project_dir="/repo/x",
  item_names=["Thing"],
  limit=50,
  include_attributes=false
)
```

## Planning

### `bbox_refactor_plan`

Creates a dry-run plan as reviewable JSON. The plan carries file hashes,
proposed text edits or moves, parse validation results, and leftovers
(items the plan couldn't handle). Read it before applying.

```
bbox_refactor_plan(
  kind="<plan_kind>",
  source="src/path/to/file.ext",
  target="src/path/to/target.ext",
  item_names=["Thing"],
  project_dir="/repo/x"
)
```

`output_path` writes the plan to disk — required when the plan body
exceeds the MCP transport limit. Pass the same path via `plan_path` to
apply.

### Generic plan kinds

| Kind | What it does |
|---|---|
| `move_file` | Move a file, updating imports where supported |
| `replace_text` | Exact text replacement (not semantic rename) |
| `write_file` | Whole-file write |
| `toml_ensure_table` | Idempotent TOML table upsert |

### Java plan kinds

Pull `sm-refactor-java` for full argument docs and caveats.

| Kind | What it does |
|---|---|
| `extract_java_methods` | Extract methods into a new or existing class |
| `extract_java_class` | Extract a class with method + field migration |
| `add_java_fields` | Add fields to a class |
| `add_java_constructor` | Add a constructor |
| `move_java_field` | Move a field to another class |
| `move_java_constant` | Move a constant |
| `add_java_delegate_field` | Add a delegate field with forwarding methods |
| `update_java_callers` | Rewrite call sites after an extraction |
| `lombokify_java_class` | Replace POJO boilerplate with Lombok annotations |

#### `lombokify_java_class` notes

Replaces trivial getters/setters, Apache Commons equals/hashCode/toString,
canonical constructors, and SLF4J `Logger log` fields with Lombok
annotations. Full-coverage classes collapse to `@Data` or `@Value`.

- `boolean_getter_strategy=bridge` — emit a `getXxx()` bridge after
  `@Getter` for `boolean` fields whose original getter used `get-`
  prefix (Lombok generates `isXxx()` by default). Use when callers
  you can't update depend on the `get-` form.
- `boolean_getter_strategy=skip` (default) — leave the field's getter
  alone; safer when API compatibility matters.
- Lombok must already be on the classpath.
- `source` can be a directory for bulk mode; `output_path` is required
  for large bulk plans.

#### `deep_analysis` flag

Pass `deep_analysis: true` on `move_java_field`, `extract_java_methods`,
or `extract_java_class` for pessimistic safety reports:

- `move_java_field` — `remaining_source_accessors`: every read/write of
  the moved field still in the source class. Non-empty means lines that
  will fail to compile.
- `extract_java_methods` / `extract_java_class` — `captured_variables`
  (source-class fields used by extracted methods), `external_calls`
  (source-class methods called by the extracted set but not in it),
  `inherited_dependencies` (superclass/interface methods reached via the
  type index).

Resolve every reported entry before applying.

## Applying

### `bbox_refactor_apply`

Apply a reviewed plan. Refuses stale hashes; validates rewritten source
files before writing; rolls back on any write failure.

```
bbox_refactor_apply(confirm=true, plan_path="/tmp/refactor.json")
# or inline:
bbox_refactor_apply(confirm=true, plan=<plan-json>)
```

`confirm=true` is required — the tool will not apply without it.

## Compound runs

### `bbox_refactor_run`

Compose multiple plans and validation commands into one transaction.
Snapshots touched files before the first write; rolls back all primitive-
plan writes if any required step fails.

```
bbox_refactor_run(
  title="extract UserRepository",
  project_dir="/repo/x",
  confirm=true,
  steps=[
    {
      "op": "plan",
      "kind": "extract_java_class",
      "source": "src/UserService.java",
      "item_names": ["UserRepository"]
    },
    {
      "op": "command",
      "command": "mvn",
      "args": ["-q", "compile"],
      "touches": []
    }
  ]
)
```

**Step kinds:**

- `op: "plan"` — a `bbox_refactor_plan` primitive. Required to succeed
  unless `optional: true` is set (turns plan-time failures into
  `skipped` entries — useful for bulk lombokify batches where some files
  have nothing to do).
- `op: "command"` — a validation command. Validation-only unless `touches`
  declares paths it may mutate; declared touches are snapshotted for
  rollback.

## Grounding entity refs

### `bbox_refactor_project_refs`

Returns current `project_file:<project>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>`
refs for a source file. Use before editing eval fixtures, provenance
metadata, or design docs that store these refs — do not guess the hash
segments.

```
bbox_refactor_project_refs(
  file="src/packets/mod.rs",
  project_dir="/repo/x",
  query="compile",
  limit=20
)
```

## Language support summary

| Language | Status |
|---|---|
| Java | Full — extract class/method, move field/constant, lombokify, add constructor/fields, update callers, JDTLS import repair |
| Rust | Status inventory + plan scaffolding |
| Other | Generic plans (file move, text replace, write, TOML) |

Tree-sitter grammars cover inspection across more languages than writable
plans. When a plan kind isn't available, use `replace_text` or `write_file`
plus manual verification.
