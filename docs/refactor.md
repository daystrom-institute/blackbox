# Refactor Tools And Atoms

Blackbox has two refactor layers.

The low layer is mechanical: tree-sitter inventory, dry-run plans, hash-checked
apply, and transactional compound runs. It knows where syntax lives and can move
or rewrite it without losing byte ranges.

The high layer is packaged as atoms. A refactor atom wraps the mechanical tools
with a prompt protocol, guardrails, validation commands, and a stable input
schema. Use the tools when you are driving the operation yourself. Use an atom
when you want a reusable refactor capability with a trace handle and clear
contract.

Full runbooks still live in system memory:

```text
bbox_knowledge(query="sm-refactor")
bbox_knowledge(query="sm-refactor-java")
bbox_knowledge(query="sm-refactor-rust")
```

## Operator Loop

For direct tool use, keep the loop boring:

```text
1. Inventory   bbox_code_symbols / bbox_refactor_status
2. Plan        bbox_refactor_plan with dry-run output
3. Review      read the plan, hashes, edits, and leftovers
4. Apply       bbox_refactor_apply(confirm=true)
5. Validate    cargo check, mvn test, javac, or the local equivalent
```

For multi-step work, use `bbox_refactor_run` so planning, writes, and validation
share one rollback boundary.

For repeatable work, install and invoke a refactor atom:

```text
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/rust-test-island-extract.json")

atom_invoke(
  atom="atom:rust-test-island-extract@v1",
  project_dir="/repo/blackbox",
  args={
    "project_dir": "/repo/blackbox",
    "source_file_or_dir": "src/tests.rs",
    "apply": false
  },
  owner="operator:me"
)
```

Plan-only first is the sane default. Flip `apply` only after the target and
guardrails are clear.

## Tool Map

| Tool | Use it for |
|---|---|
| `bbox_code_symbols` | Find methods, functions, types, and modules across a project with real line ranges. |
| `bbox_code_query` | Run a tree-sitter query against one file once you know the grammar shape. |
| `bbox_code_node_describe` | Inspect the node at a line/column before writing a query or plan. |
| `bbox_refactor_status` | Check parse health and list refactorable items in one file. |
| `bbox_refactor_project_refs` | Get current `project_file:` refs for provenance or eval fixtures. |
| `bbox_refactor_plan` | Produce reviewable JSON without touching files. |
| `bbox_refactor_apply` | Apply one reviewed plan with hash checks and rollback. |
| `bbox_refactor_run` | Run multiple plans plus validation commands transactionally. |

Use `rg` for plain text search. Use these tools when identity matters: item
kind, byte range, imports, call sites, moved fields, or compile repair.

## Inventory

Start with symbols when you do not know the file:

```text
bbox_code_symbols(
  project_dir="/repo/x",
  query="readFromProperties",
  languages=["java"],
  item_kinds=["method_declaration"],
  limit=20
)
```

Use status once you do know the file:

```text
bbox_refactor_status(
  file="src/path/to/File.java",
  project_dir="/repo/x",
  item_names=["Thing"],
  limit=50,
  include_attributes=false
)
```

Use node describe before writing a tree-sitter query from memory:

```text
bbox_code_node_describe(file="src/foo.rs", line=42, column=12)
```

That call is cheap, and it prevents the usual grammar-name guessing.

## Planning

`bbox_refactor_plan` is a dry run. It returns hashes, proposed edits, file moves,
parse checks, and leftovers. Read the leftovers. They are where bad refactors
usually hide.

```text
bbox_refactor_plan(
  kind="<plan_kind>",
  source="src/path/to/file.ext",
  target="src/path/to/target.ext",
  item_names=["Thing"],
  project_dir="/repo/x"
)
```

Use `output_path` for large plans and pass that path to apply:

```text
bbox_refactor_plan(..., output_path="/tmp/refactor-plan.json")
bbox_refactor_apply(confirm=true, plan_path="/tmp/refactor-plan.json")
```

The apply tool refuses stale hashes. If someone edits the file after planning,
re-plan instead of forcing it.

## Plan Kinds

Generic:

| Kind | What it does |
|---|---|
| `move_file` | Move a file and update imports where supported. |
| `replace_text` | Exact replacement. This is not semantic rename. |
| `write_file` | Whole-file write. |
| `ensure_toml_table` | Idempotent TOML table upsert. |

Java:

| Kind | What it does |
|---|---|
| `extract_java_methods` | Move selected methods into a new or existing class. |
| `extract_java_class` | Extract a cohesive class with method, field, delegate, caller, and visibility wiring. |
| `extract_java_nested_classes` | Move static/implicitly-static nested types to top-level classes. |
| `promote_java_inner_class` | Promote a non-static inner class with outer captures to a top-level class. |
| `add_java_fields` | Add fields to a class. |
| `add_java_constructor` | Add a constructor. |
| `move_java_field` | Move a field to another class. |
| `move_java_constant` | Move a constant. |
| `add_java_delegate_field` | Add a delegate field with forwarding methods. |
| `update_java_callers` | Rewrite call sites after an extraction. |
| `rewrite_java_visibility` | Rewrite method visibility to public/protected/private/package. |
| `add_java_implements` | Add an implements clause to a Java class. |
| `extract_java_interface` | Extract an interface, add implements, and widen selected methods as needed. |
| `migrate_java_type_usages` | Rewrite structural type-use positions from concretion to interface/type. |
| `java_lsp_organize_imports` | Organize imports through warm JDTLS with structural fallback. |
| `find_java_usages` | Analysis-only project-wide reference walk for simple Java names. |
| `rename_java_symbol` | Project-wide syntax rename for Java declarations and references. |
| `java_class_dependency_analysis` | Analysis-only class dependency graph for partition decisions. |
| `java_public_api_guard` | Analysis-only public API delta advisory before mutating refactors. |
| `lombokify_java_class` | Replace POJO boilerplate with Lombok annotations. |

Rust support is still more atom-led than primitive-led. Inventory and generic
plans are available; the shipped Rust refactor atoms carry the higher-level
protocols.

## Java Notes

Use `deep_analysis: true` on `move_java_field`, `extract_java_methods`, or
`extract_java_class` when the change crosses state or caller boundaries.

The report fields are meant to stop you:

| Field | Why it matters |
|---|---|
| `remaining_source_accessors` | Reads/writes of a moved field still left behind. Non-empty usually means compile failure. |
| `captured_variables` | Source-class fields used by extracted methods. Decide whether they move, become constructor args, or stay delegated. |
| `external_calls` | Source-class methods called by the extracted set but not included. |
| `inherited_dependencies` | Superclass or interface methods reached through the type index. |

For Lombok:

- Lombok must already be on the classpath. The tool does not add the dependency.
- `boolean_getter_strategy=bridge` preserves callers that expect `getXxx()` for
  boolean fields.
- `boolean_getter_strategy=skip` keeps the original getter. This is safer when
  public API compatibility matters.
- Bulk mode requires `output_path`; review the skipped entries before applying.

## Compound Runs

`bbox_refactor_run` is the right tool when a refactor needs more than one plan or
when validation needs to be part of rollback.

```text
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

Plan steps write through the same hash-checked apply path. Command steps are
validation-only unless `touches` declares files they may mutate. Declared touches
are snapshotted for rollback.

## Refactor Atoms

Refactor atoms live under `system-defaults/atoms/refactor/`. Install the ones you
actually use. [Artifact Catalog And System Defaults](artifact-catalog.md) covers
general install order and supersession; this section focuses on refactor choices:

```text
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/java-refactor-persona.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-extract-cohesive-class.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/rust-split-god-impl.json")
```

They are profile-backed atoms, so each invocation starts a brofile-backed
provider task and returns a resumable atom handle. The atom input schema is the
contract; the prompt template inside the manifest is the runbook the dispatched
agent follows.

### Java Refactor Atoms

| Atom | Job |
|---|---|
| `atom:java-class-dependency-graph@v2` | Inventory a Java class before deciding what can be extracted. |
| `atom:java-extract-cohesive-class@v3` | Extract methods plus field moves, delegate wiring, caller rewrites, and visibility fixes. |
| `atom:java-extract-interface@v2` | Extract an interface and optionally migrate callers to the interface type. |
| `atom:java-find-usages@v1` | Walk references for one or more Java names. |
| `atom:java-lombokify@v1` | Convert hand-written POJO boilerplate to Lombok where semantics are safe. |
| `atom:java-promote-inner-class@v1` | Promote a non-static inner class with outer captures to a top-level class. |
| `atom:java-public-api-guard@v1` | Report public API impact before a Java refactor. |

### Rust Refactor Atoms

| Atom | Job |
|---|---|
| `atom:rust-error-migrate@v1` | Rewrite a Rust module's error type and repair construction / `?` sites. |
| `atom:rust-impl-partition-graph@v1` | Produce a method/field/call graph for a large impl block. |
| `atom:rust-public-api-guard@v1` | Report public API impact before a Rust refactor. |
| `atom:rust-split-god-impl@v1` | Split a multi-domain impl block into modules with router wiring and visibility updates. |
| `atom:rust-state-extract@v1` | Pull a cluster of `self.field` reads into a separate state struct. |
| `atom:rust-test-island-extract@v1` | Move inline `#[cfg(test)] mod tests` blocks into sibling `src/tests/*.rs` modules. |
| `atom:rust-trait-from-impl@v1` | Lift selected inherent methods into a trait plus impl. |

### Atom Operating Rules

- Invoke with `apply=false` first when the atom supports it.
- Read `atom_status` before resuming. Provider-backed handles may need a moment
  before the real session id is available.
- Delegate before asking another operator or reviewer to resume.
- Treat `bbox_note(kind="blocked")` from an atom as a real stop, not advisory
  prose.
- If the atom produces a plan path, inspect the plan before applying or asking it
  to continue.

## Grounding Entity Refs

Use `bbox_refactor_project_refs` before editing eval fixtures, provenance
metadata, or design docs that store `project_file:` refs.

```text
bbox_refactor_project_refs(
  file="src/packets/mod.rs",
  project_dir="/repo/x",
  query="compile",
  limit=20
)
```

Do not guess hash segments. They encode the current indexed file/chunk identity.

## Language Support

| Language | Practical status |
|---|---|
| Java | Mature primitive support plus refactor atoms. Extract class/method/interface, nested/inner type extraction, move field/constant, visibility/type migration, find usages, rename, dependency/API analysis, lombokify, caller updates, and JDTLS import repair are wired. |
| Rust | Inventory, generic plans, and a growing atom catalog for higher-level transformations. |
| Other | Inspection plus generic file/text/TOML plans. |

When the primitive plan kind does not exist, use an atom if one matches the job.
If neither exists, fall back to `replace_text` / `write_file` and run the local
compiler or test suite yourself.
