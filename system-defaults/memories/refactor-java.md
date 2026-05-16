---
tags:
  - refactor-tools
  - java
---
+++
title = "Java refactor mechanization — tree-sitter inventory and JDT validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "java", "jdt", "jdtls", "intellij", "eclipse", "maven", "gradle", "tree-sitter", "bbox_refactor_status", "bbox_refactor_plan", "bbox_refactor_apply", "bbox_refactor_run", "bbox_code_refs", "symbol", "rename", "move", "extract", "extract_java_methods", "extract_java_class", "extract_java_nested_classes", "promote_java_inner_class", "extract_java_interface", "add_java_implements", "migrate_java_type_usages", "java_lsp_organize_imports", "rewrite_java_visibility", "find_java_usages", "rename_java_symbol", "java_class_dependency_analysis", "java_public_api_guard", "lombokify_java_class", "prune_java_orphans", "extract_java_code_block_to_method", "convert_method_to_class", "inline_java_method", "extract_java_test_slice", "java_collapse_call_chain", "sm-refactor-java-extract-class", "sm-refactor-java-lombokify"]
order = 12
template = false
+++
# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Atom signposts

For recurring Java refactor patterns, check `atom_search(query="<intent>")`
before re-deriving the whole tool sequence. Use atoms as contextual shortcuts
for patterns such as class dependency inventory, public-API preflight,
project-wide usage enumeration, cohesive-class extraction, inner-class
promotion, interface extraction, lightweight method moves, source-local caller
rewrites, low-level field/constructor/delegate wiring, visibility rewrites,
type-use migration, import organization, Java symbol rename, or Lombok
conversion. The atom manifest is the source of truth for version, inputs, cost,
and prompt text; this memory keeps the primitive plan-kind map and safety
invariants.

When no atom fits the exact shape, the manual plan-kind sequence below is the
canonical path.

## Current Capability

Java has full inspect-and-extract support, plus composite class extraction,
field/constructor wiring, caller delegation, interface extraction, visibility
rewriting, type migration, import organization, and Lombok-ification of
hand-rolled boilerplate (POJO DOJO).

Every Java refactor plan kind has at least one shipped refactor atom wrapper
under `system-defaults/atoms/refactor/`. The coverage guard
`java_refactor_plan_kinds_have_atom_coverage` fails if a new Java plan kind is
added without corresponding atom coverage. Use `atom_search(query="<intent>",
subcontract="refactor/v1")` for the current catalog instead of relying on this
memory as an atom ledger.

- Inspect: supported with `bbox_refactor_status`; syntax grounding also uses
  `bbox_code_symbols`, `bbox_code_node_describe`, `bbox_code_query`, and
  `bbox_code_refs` from the shared `sm-refactor` toolkit.
- Plan/apply: method extraction, composite class extraction, nested class extraction, field moves/adds, constructor creation, delegate-field wiring, caller delegation, interface extraction, visibility rewriting, implements clause injection, type-use migration, import organization, and `lombokify_java_class` (POJO boilerplate → Lombok annotations).
- Find usages: supported with `bbox_refactor_plan(kind="find_java_usages")`.
  Walks every `.java` file under `project_dir` and reports
  AST-grounded references (type position, method invocation, field
  access, method reference, import) for the supplied simple name(s).
  Analysis-only; no FileEdits. Foundation for the semantic-rename
  primitive below. Optional params:
  - `declaring_class: String` — filter results to usages whose
    receiver expression plausibly resolves to a field of that type
    on the enclosing class. The current heuristic walks the enclosing class
    for `private final <DeclaringClass> <field>;` declarations and
    keeps only call sites whose receiver identifier is one of those
    field names. Use this whenever the simple name isn't globally
    unique (common method names like `getId`, `getName` return
    hundreds of unrelated sites without it).
  - `output_path: String` — write the full report JSON to a filename
    under `$BLACKBOX_STATE_DIR/refactor/plans/` and return a compact
    digest (`total_usages`, `unique_call_files`,
    `usage_summary_by_name`) instead. Avoids MCP transport-cap
    blowups on common names.
  - `summary_only: bool` — return per-name counts + top-5 example
    call sites only. Combines with `output_path`.
  - Each usage entry carries `is_test_site: bool` (true when the
    file's path has a `src/test/java/` ancestor). Top-level response
    carries `production_sites` and `test_sites` tallies.
- Semantic rename: supported with `bbox_refactor_plan(kind="rename_java_symbol")`.
  Project-wide rename of a class / interface / record / enum / method
  / field / parameter / type-parameter by simple name. Rewrites
  declaration sites AND every reference (type position, method
  invocation, field access, method reference, import). When the
  renamed symbol is a top-level class declared in a file matching the
  old name, the response surfaces a `file_rename_advisory` listing
  the suggested `OldName.java` → `NewName.java` rename — the operator
  follows up with `move_file` or `git mv`. Optional `item_kinds`
  narrows which categories of declaration get touched (e.g. only
  `class_declaration` + `type_reference` to leave a same-named local
  variable alone). For semantic-grade accuracy on disambiguation
  (e.g. method overloads that share a name), JDTLS-backed rename is
  the right escape hatch — but the standalone primitive lands every
  reference its tree-sitter walker can see.
- Import/package repair: `java_lsp_organize_imports` prefers a warm
  per-project JDTLS session (lazy-spawned, reused across calls, idle-evicted
  by the daemon) and falls back to tree-sitter plus project type scanning
  when JDTLS is unavailable or returns no edits. The fallback also keeps
  inner-class references in qualified `Outer.Inner` form.

Tree-sitter language: `java`.

## Child memories — when to pull alongside this one

- `sm-refactor` — parent invariants: shared code-nav semantics, supported plan
  kinds by language, refactor persona tool surface, and the atom
  install/invoke contract.
- `sm-refactor-java-extract-class` — `extract_java_class` capability
  checklist, parameter reference (`wiring_mode`, `source_delegate_wrappers`,
  `propagate_class_annotations`, `callback_externals`, etc.), capture-analysis
  and accessor-rewrite semantics, and the catalog of operator-facing FIXME
  markers the planner emits when it cannot resolve a dependency on its own.
- `sm-refactor-java-lombokify` — `lombokify_java_class` plan kind: detection
  table, collapse rules, conservative refusals, `boolean_getter_strategy`,
  bulk mode, plan-to-file, classpath prerequisites, curated-batch composition.

This parent also keeps concise sections for `promote_java_inner_class`,
`extract_java_nested_classes`, `find_java_usages`, `rename_java_symbol`,
`extract_java_interface`, `migrate_java_type_usages`, low-level field /
constructor / delegate / caller primitives, and `java_lsp_organize_imports`.
Split those into dedicated child memories when the detail grows beyond quick
routing/signpost prose.

All child memories are reachable via `bbox_knowledge(query=...)` with their own
tag set. Pull the relevant child for parameter detail; this parent stays a
language-level map of plan kinds, the tool sequence, safety rules, and
contextual atom signposts.

## `promote_java_inner_class` — for clusters with capture-aware inner classes

When a cluster you want to extract includes a non-static inner class that
captures outer-class state, use `promote_java_inner_class` BEFORE running
`extract_java_class` on the outer methods. (The static-inner case is
covered by `extract_java_nested_classes` — that's a syntactic move with
no capture analysis.)

```text
bbox_refactor_plan(
  kind="promote_java_inner_class",
  source="src/main/java/com/example/Outer.java",
  target="src/main/java/com/example/.../Promoted.java",
  module_name="Promoted",
  item_names=["Promoted"],   # same value as module_name; both accepted
  project_dir="/repo/x"
)
```

What it does:

- Walks the inner class body for outer-field reads. Bare `field` (after
  shadow checks against inner fields, locals, params) and
  `OuterClass.this.field` both count as captures.
  `this.field` is NEVER a capture (it binds to the inner instance only;
  lambdas inherit enclosing `this`, but anonymous-class bodies rebind
  `this` and are detected accordingly).
- Synthesizes or augments a single constructor on the promoted class
  with `final` captures as parameters. Captures are assigned AFTER any
  leading `super(...)` chain.
- Rewrites every `new <Inner>(args)` site in source to
  `new <Promoted>(args, capture1, capture2, ...)`.
- Drops the inner declaration from source.
- Adds `import <target-package>.<Promoted>;` on cross-package targets.

Refusal codes (the planner returns these instead of emitting broken Java):

| Code | When |
|------|------|
| `static_inner_class_in_promote` | Inner is declared `static`. Use `extract_java_nested_classes` for a syntactic move; static inners have no outer captures. |
| `inner_class_writes_outer_field` | Inner writes (assigns / increments) an outer field. `final` ctor params can't be reassigned. Refactor the write before promoting. |
| `inner_class_calls_outer_method` | Inner calls a source-class method. The planner does not thread outer-method calls; refactor (inline or accept a `Runnable` callback) before promoting. |
| `inner_class_multiple_ctors` | Inner has more than one constructor. Consolidate first. |
| `inner_class_this_chain_ctor` | Inner's ctor delegates via `this(...)`. Inline the delegation first. |
| `inner_class_referenced_as_type` | Inner is referenced outside `new <Inner>(...)` (variable decl, cast, method reference, `Outer.Inner` path). The planner only rewrites instantiations; handle other sites manually. |

Workflow after promotion: run `extract_java_class` on the outer cluster
as a separate call. The moved methods will reference the promoted class
via the source's new import.

## `extract_java_nested_classes` — for static inner classes

When a cluster you want to extract includes one or more **static** inner
classes (no outer captures), use `extract_java_nested_classes`. This is
a syntactic move — no capture analysis, no delegate wiring.

```text
bbox_refactor_plan(
  kind="extract_java_nested_classes",
  source="src/main/java/com/example/Outer.java",
  target="src/main/java/com/example/queries/Readings.java",
  item_names=["Readings"],
  project_dir="/repo/x"
)
```

What it does:

- **Filesystem-derived `package` declaration** on the target, same
  rules as `extract_java_class`. Operator can override with explicit
  `target_prelude`.
- **Copies imports from the source** so the extracted body's type
  references resolve. The full import block is duplicated; unused
  imports get cleaned up by a subsequent `java_lsp_organize_imports`
  call if desired.
- **Strips `private` and `static` modifiers** on the moved class
  becoming top-level. A top-level Java class can be neither
  `private` nor (meaningfully) `static`; javac rejects both. `final`,
  `abstract`, `sealed`, `non-sealed`, and annotations pass through
  untouched.
- **Cross-package extracts inject `public`** before the existing
  modifiers (or before the `class`/`interface`/`record`/`enum`
  keyword if no modifiers), so the source's qualified references
  still resolve from the new package. Same-package extracts keep
  package-default visibility — operator widens afterwards if needed.
- **Tree-sitter validation steps cover both source and target**, so
  regressions surface at plan time.

Use this kind when:

- The inner class is `static` and has no outer-state captures.
  (Non-static inners need `promote_java_inner_class` first — see above.)
- You want a clean file-per-class layout for things like sibling
  `queries/` packages.

`extract_java_class` itself refuses inner-class names in `item_names`
with `error.bad_input(code=nested_class_in_item_names)` — that's
correct; use this plan kind instead for the inner-class move, then run
`extract_java_class` on the outer cluster.
- **`bbox_refactor_plan` always returns `dry_run: true`.** The response
  field indicates "this call did not write any files" — read it as the
  inverse of `wrote_files`. The plan is staged on disk under
  `$BLACKBOX_STATE_DIR/refactor/plans/<name>.json` (when `output_path`
  was passed) and is applied via a follow-up `bbox_refactor_apply`.
- **`plan_path` round-trips.** The absolute path returned in the plan
  response (e.g. `/home/.../refactor/plans/extract.json`) is accepted
  verbatim by `bbox_refactor_apply(plan_path=...)`. Relative filenames
  also work. Slot-escaping paths (`/tmp/...`, `../../etc/passwd`) are
  still rejected.
- **Import inference for static-call and method-reference receivers.**
  The organize-imports heuristic walking the generated target's AST
  recognizes type names used as the receiver of `method_invocation` /
  `field_access` *and* as the qualifier of `method_reference` nodes
  (uppercase-initial identifier) in addition to `type_identifier`. JDK
  / Vaadin / project-local types accessed as `Collectors.toList()`,
  `BigDecimal.ZERO`, `DateUtils.parse(...)`, or
  `FormCategoryEnumConverter::toLabel` get their imports retained from
  the source or added from the project type index.

## Tool Sequence

1. Find Java methods/types and line ranges across the project:

```text
bbox_code_symbols(
  project_dir="/absolute/project/root",
  query="readFromProperties",
  languages=["java"],
  item_kinds=["method_declaration"],
  limit=20
)
```

Use this instead of `rg -n` for method, constructor, class, interface, record,
or enum line numbers in supported Java source. It returns exact `line_range`,
byte range, item kind/name, and handoff calls for `bbox_refactor_status` and
`bbox_refactor_project_refs`.

2. Inventory a file:

```text
bbox_refactor_status(
  file="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level type
declarations, Java `method_declaration` / `constructor_declaration` items,
nested type declarations, names where tree-sitter exposes them, byte ranges, and
line ranges. For method extraction, copy exact method names from this inventory
or from `bbox_code_symbols`, `bbox_code_query`, or `bbox_code_node_describe`
handoff suggestions.

3. Extract methods into a new or existing class:

```text
bbox_refactor_plan(
  kind="extract_java_methods",
  source="src/main/java/com/example/GodClass.java",
  target="src/main/java/com/example/ExtractedMethods.java",
  module_name="ExtractedMethods",
  item_names=["myMethod1", "myMethod2"],
  project_dir="/absolute/project/root"
)
```

For `extract_java_methods`, the target class file may be missing. In that case
the plan creates it automatically with a `public class` wrapper, using
`module_name` as the class name or the target file stem if `module_name` is
omitted. It copies the source package declaration by default; pass
`target_prelude` when the extracted class needs a different package/import
header. Do not pre-create an empty target file just to satisfy the planner, and
do not use `allow_dirty_worktree=true` for this normal create-target flow.

The plan also reports `captured_variables` for source-class fields referenced
by moved methods. Use that report to decide which fields to move, which fields
to recreate on the target, and which dependencies should become constructor
parameters.

Capture resolution rules: `captured_variables` only contains
identifiers that resolve to a direct `field_declaration` of the **outer
source class**. Method parameters, local variables, enhanced-for variables,
and inner-class fields are not captures — they either travel with the method
or live in a separate scope. Bare-name reads inside the method body are
shadowing-checked against enclosing locals/parameters; only `this.<name>`
accesses bypass shadowing. This stops false captures like a method parameter
named the same as an inner-class field from being promoted into a constructor
parameter on the target.

Each capture also carries two mutability indicators:
- `source_mutable: true` when the source field is declared without `final`.
  Promoting a mutable field to a `final` constructor parameter snapshots its
  value at construction time and the target sees stale data after later
  source-side writes — surface a warning to the operator.
- `source_static_final: true` when the source field is `static final`. The
  composite plan should treat these as constants (move with initializer
  preserved via `move_java_constant`) rather than promoting them to instance
  fields on the target.

Both flags default to `false` when omitted from the JSON (serialization
elides `false` via `skip_serializing_if`). Defaulting to `false` matches the
common case of plain `private` instance fields and keeps the safer "treat as
mutable" warning live whenever the modifier walk fails.

Pass `deep_analysis: true` on the plan call to also receive `external_calls`
and `inherited_dependencies`. The flag is opt-in because the inherited-method
walk crosses files via the project type index; default `false` keeps the
response lean for self-contained clusters where the operator already knows
the extraction is clean. Set it to `true` whenever the cluster touches
methods from the source class, lambdas that capture `this`, or methods from
a superclass / implemented interface — those are the silent-miscompile
risks the report surfaces:

- `external_calls` lists method invocations inside the extracted set that
  resolve to methods on the source class but are NOT in the extracted set.
  Each entry carries `method`, a best-effort `signature` (with
  `signature_partial: true` when the declaring node could not be cleanly
  recovered), and `call_sites`. Each call site has `line`, `column`,
  `in_method`, and `context` (`"direct"` or `"lambda"` — lambdas
  capture `this` differently and may need a closure over a parent reference
  rather than a simple delegate).
- `inherited_dependencies` lists method invocations that resolve to a
  superclass or implemented-interface method declared elsewhere in the
  project type index (BFS through `extends`/`implements`, cycle-guarded).
  Each entry carries `method`, `source` (declaring type name), `source_kind`
  (`"class"` or `"interface"`), and the same `call_sites` shape with
  `context`.

Calls that don't resolve in the project type index are dropped — they're
likely JDK or third-party library methods, and the target file's existing
imports already cover them. Calls with explicit non-`this` receivers are also
dropped. Resolve each finding before applying:

| Finding | Resolution |
|---------|-----------|
| `external_calls` | Add the method to `item_names`, extract a callback interface with `extract_java_interface`, or pass the source instance through. |
| `inherited_dependencies` | Add the same `implements` / `extends` to the target, or inject the dependency (e.g. `Logger`) via the constructor. |


4–9. Composite extract — see `sm-refactor-java-extract-class` for
     `extract_java_class` (one-shot composite extraction with delegate
     wiring + caller rewrite) and for the finer-grained primitives
     (`add_java_fields`, `add_java_constructor`, `move_java_field`,
     `move_java_constant`, `add_java_delegate_field`, `update_java_callers`),
     plus the full parameter
     reference, capture-analysis semantics, accessor-rewrite rules,
     and the generated-FIXME marker catalog operators consult after
     apply.

Low-level atom wrappers exist for each primitive in this block:
`java-add-fields`, `java-add-constructor`, `java-move-field`,
`java-move-constant`, `java-add-delegate-field`, and `java-update-callers`.
Prefer `java-extract-cohesive-class` when the desired shape is a normal
extract-class flow; use these atoms when the operator is deliberately composing
leaf primitives.

10. Extract an interface from a class:

Creates a new interface file with method signatures, adds `implements` on the source class, and widens non-public methods to `public` as needed.

```text
bbox_refactor_plan(
  kind="extract_java_interface",
  source="src/main/java/com/example/ServiceImpl.java",
  target="src/main/java/com/example/Service.java",
  item_names=["process", "validate"],
  module_name="Service",
  project_dir="/absolute/project/root"
)
```

Parameters:
- `source` — class to extract from.
- `target` — path for the new `.java` interface file.
- `module_name` — interface name (defaults to class name; strips "Default" prefix if present).
- `impl_name` — optional class name to target if file has multiple classes.
- `item_names` — optional method names to include; defaults to all public non-static methods.

11. Add `implements` clause to a class:

```text
bbox_refactor_plan(
  kind="add_java_implements",
  source="src/main/java/com/example/ServiceImpl.java",
  module_name="Service",
  impl_name="ServiceImpl",
  project_dir="/absolute/project/root"
)
```

Parameters:
- `module_name` — interface name to add.
- `impl_name` — optional class name to target if file has multiple classes (defaults to first class).

12. Rewrite method visibility:

```text
bbox_refactor_plan(
  kind="rewrite_java_visibility",
  source="src/main/java/com/example/Thing.java",
  item_names=["internalMethod", "helperMethod"],
  visibility="public",
  project_dir="/absolute/project/root"
)
```

`visibility` must be one of: `public`, `protected`, `private`, `package` (removes keyword).
The matching atom is `java-rewrite-visibility`; it preflights public API impact
with `java_public_api_guard` before applying.

13. Migrate type usages (concretion -> interface):

Replaces type-use positions (variable declarations, parameters, return types, field types) while skipping `new`, method calls, `.class`, `instanceof`, and cast positions.

```text
bbox_refactor_plan(
  kind="migrate_java_type_usages",
  source="src/main/java/com/example/Client.java",
  module_name="ServiceImpl",
  new_text="Service",
  project_dir="/absolute/project/root"
)
```

The matching atom is `java-migrate-type-usages`; it pairs
`migrate_java_type_usages` with `java_lsp_organize_imports` and a public API
preflight.

14. Organize imports:

```text
bbox_refactor_plan(
  kind="java_lsp_organize_imports",
  source="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```

The matching atom is `java-organize-imports`.

The planner asks JDTLS for workspace-aware organize-import edits through a
shared per-project session pool. The first call for a `(project_dir, java)`
pair lazily spawns JDTLS, awaits a real `initialize` response (no fixed
sleep), sends `initialized`, then **waits for the workspace to actually
import** — JDTLS streams `language/status` notifications during gradle /
maven / Buildship project import and signals `{ type: "Started" }` (Eclipse
JDT.LS) or emits a Buildship `language/eventNotification { eventType: 100 }`
when classpath resolution finishes. Until that drain completes, the session
is "ready" by LSP-protocol semantics but JDTLS hasn't loaded a single
project class — organize-imports can't tell whether statics are used,
references return empty, and edits arrive with degenerate whitespace
boundaries. The pool blocks on that signal up to
`BLACKBOX_JDTLS_READY_TIMEOUT_SECS` (default 60s); on timeout the session
proceeds and the caller sees degraded results rather than a hang.

Per-file, the organize-imports call also opens the source via `didOpen` and
waits for the first `publishDiagnostics` for that URI before issuing the
code-action request — mirrors the rust-analyzer post-`didOpen` drain so the
classpath-resolved request is what JDTLS actually sees, not a request that
races ahead of JDTLS's lazy per-file type-check.

Subsequent calls reuse the same long-lived child. Idle sessions are evicted
on a 60s tick after `BLACKBOX_LSP_IDLE_SECS` (default 600) of inactivity,
and the daemon shuts every session down on stop. Tunables:
`BLACKBOX_JDTLS_INIT_TIMEOUT_SECS` (default 60) for the cold-start window,
`BLACKBOX_JDTLS_READY_TIMEOUT_SECS` (default 60) for the post-`initialized`
workspace-import drain, `BLACKBOX_JDTLS_TIMEOUT_SECS` (default 30) per
request, `BLACKBOX_JDTLS_BIN` to point at a non-default binary.

If JDTLS is absent, the session is broken, or the request returns no edits,
the tool falls back to a structural project scan: removes plain imports
whose simple names are no longer referenced, keeps static and wildcard
imports, and adds imports for uniquely named Java source files in the same
`project_dir` when their simple type name is referenced. The fallback also
detects inner-class-only simple names and skips synthesizing imports for
them — references like `Outer.Inner` keep their qualified form rather than
producing a non-resolving `import x.Inner;`. The fallback is a structural
heuristic, not a classpath resolver — it cannot identify unused static
imports from third-party jars; only the LSP-backed path can, and only after
the workspace-import drain has completed.

14b. Lombokify hand-rolled POJO boilerplate — see
    `sm-refactor-java-lombokify` for `lombokify_java_class` (single-file
    + bulk + curated-batch), the detection table, collapse rules,
    refusal heuristics, `boolean_getter_strategy`, and prerequisites.
    Lombok must already be on the classpath; the planner does not
    modify the build.

14b6. Collapse verbose getter chains:

```text
bbox_refactor_plan(
  kind="java_collapse_call_chain",
  source="src/main/java/com/example/View.java",
  project_dir="/absolute/project/root",
  impl_name="SessionData",
  module_name="getAuthLogRecord.getAuthLogId",
  new_text="getAuthLogId"
)
```

The matching atom is `java-collapse-call-chain`. Walks the file for
`<recv>.<A>().<B>()` chains where `<recv>` is a `private final
<impl_name> <field>` on the enclosing class (find_java_usages
receiver-resolution heuristic) and `<A>` / `<B>` match the
`module_name` segments. Replaces each match with `<recv>.<new_text>()`.

v1 supports exactly two-segment chains with no-arg intermediate calls.
Side-effect-aware refusal: chains with arguments anywhere in the inner
or outer call are skipped to avoid losing side-effecting calls. v2:
multi-step chains, project-wide walks, method-reference support.

14b5. Migrate tests after an extract_java_class production move:

```text
bbox_refactor_plan(
  kind="extract_java_test_slice",
  source="src/test/java/com/example/BigTest.java",
  target="src/test/java/com/example/AdderTest.java",
  project_dir="/absolute/project/root",
  item_names=["add","subtract"]
)
```

The matching atom is `java-extract-test-slice`. Walks `@Test` methods
in the source test class and classifies each by which production
methods it invokes: only-moved → migrate, only-kept → leave, mixed →
refuse cleanly. The operator must split mixed-coverage tests via
`extract_java_methods` on the test class before re-running. v1 does not
synthesize Mockito stubs in remaining-source tests; that's a v2
follow-up.

14b4. Inline a private helper into its call sites:

```text
bbox_refactor_plan(
  kind="inline_java_method",
  source="src/main/java/com/example/Calc.java",
  project_dir="/absolute/project/root",
  module_name="add"
)
```

The matching atom is `java-inline-method`. Finds every `method_invocation`
of the named method (no receiver or `this` receiver) in the same file,
substitutes each call's argument expressions (wrapped in parens for
operator-precedence safety) into the method's body expression, replaces
the call expression with the substituted form, and deletes the method
declaration.

**v1 scope**: private methods only, single-statement bodies only
(return-expression for non-void, expression-statement for void). The
body must reference only formal parameters — no `this`, `super`, field
reads, or calls to other methods. Refuses cleanly on every violation;
operator can lift state to parameters first or pick a different
refactor. `inline_java_class` is a separate primitive not yet shipped.

14b3. Method Object — convert a method into a standalone class:

```text
bbox_refactor_plan(
  kind="convert_method_to_class",
  source="src/main/java/com/example/Big.java",
  project_dir="/absolute/project/root",
  module_name="processOrder",
  target="src/main/java/com/example/ProcessOrderOperation.java"
)
```

The matching atom is `java-convert-method-to-class`. Generates a new
class with private final fields for each parameter, a constructor that
assigns them, and an `execute()` method carrying the original body
verbatim (including throws clause). The original method body becomes
`return new <Class>(<args>).execute();` (or void variant). New class
name defaults to PascalCase of the method name + `"Operation"`;
override via `new_text`.

**v1 limitations**: the generated class has no reference to the
enclosing instance. Original-body uses of `this.<field>` or `super.<x>`
get flagged with a `// FIXME(method-object):` header inside `execute()`
and counted in `fixme_count.warning`. The operator either lifts those
references to parameters first or hand-edits the generated class to
take an `Outer` constructor argument. v2 auto-capture is a planned
follow-up. Refuses static methods, abstract methods, constructors, and
interface methods.

14b2. Intra-method extraction — lift a statement range into a private helper:

```text
bbox_refactor_plan(
  kind="extract_java_code_block_to_method",
  source="src/main/java/com/example/Big.java",
  project_dir="/absolute/project/root",
  old_text="<exact statement-range text>",
  module_name="<new helper name>",
  parameters=[{"type":"int","name":"x"},{"type":"String","name":"s"}],
  toml_entries={"arguments":["x","s"], "return_type":"int", "return_var":"total"}
)
```

The matching atom is `java-extract-code-block`. Finds the enclosing
`method_declaration` (or `constructor_declaration`) by walking up from
the `old_text` match position, generates a `private` helper appended
after that method, and replaces the original range with a call to the
helper. Static-ness is inherited from the enclosing method; visibility
defaults to `private` and can be overridden with `visibility="protected"`
etc. The synthesized call site is `<helper>(<args>);` for void returns
and `<return_type> <return_var> = <helper>(<args>);` for non-void. Use
`new_text` for unusual call-site shapes (chained call, try/catch wrap).

**v1 is manual mode**: the operator owns the parameter and argument
lists. The planner does NOT yet infer captures from the AST — if the
extracted range reads three locals and you supply zero parameters, the
helper compiles but won't see those locals. v2 inference (capture /
mutated-capture / multi-return analysis) is a planned follow-up.

14c. Janitor pass — delete unused private members:

```text
bbox_refactor_plan(
  kind="prune_java_orphans",
  source="src/main/java/com/example/Big.java",
  project_dir="/absolute/project/root"
)
```

The matching atom is `java-prune-orphans`. Walks every `private`
method, field, and inner class in the source file, counts in-file
references (identifier / type_identifier / method_invocation.name /
field_access.field / method_reference on either side), and proposes
delete edits for the zero-reference set. Conservative — over-counts
references rather than under-counts so live members are never deleted.

Pinning exclusions (kept even with zero textual references): constructors,
`serialVersionUID`, and members annotated with `@Override`, `@Inject`,
`@Resource`, `@Autowired`, `@PostConstruct`, `@PreDestroy`, `@Test`,
`@ParameterizedTest`, `@RepeatedTest`, `@BeforeEach`, `@AfterEach`,
`@BeforeAll`, `@AfterAll`, `@BeforeClass`, `@AfterClass`, or
`@SuppressWarnings("unused")`. Multi-declarator field declarations
(`private int a, b, c;`) are excluded in v1 — split manually and re-run.
Inputs:
- `source` — Java file to prune.
- `item_kinds` — optional subset of `["method", "field", "inner_class"]`.
- `item_names` — optional whitelist; when set, only candidates whose
  simple name is in this list are considered.

Recursive-only private methods stay alive (the self-call counts as a
reference). Reflection-accessed members are invisible to the AST walk
— annotate them with `@SuppressWarnings("unused")` before running.


15. Compound run — full extract-interface flow with rollback:

```text
bbox_refactor_run(
  title="Extract Service interface from ServiceImpl",
  project_dir="/absolute/project/root",
  confirm=true,
  steps=[
    {"op":"plan","kind":"extract_java_interface","source":"src/.../ServiceImpl.java","target":"src/.../Service.java","module_name":"Service"},
    {"op":"plan","kind":"migrate_java_type_usages","source":"src/.../Client.java","module_name":"ServiceImpl","new_text":"Service"},
    {"op":"command","command":"mvn","args":["compile","-pl","."]}
  ]
)
```

16. Validate with project commands:

```text
mvn test
./mvnw test
gradle test
./gradlew test
```

## Safety Rules

- Do not apply Rust plan kinds to Java files.
- Tree-sitter does not enforce package/path consistency, generic type binding, annotation processing, Lombok/generated code, or classpath semantics.
- **Annotation-processor-generated members are invisible to dependency analysis.** The `inherited_dependencies` walk traverses `extends` / `implements` chains in the project type index — it does not run annotation processors. Class-level annotations that generate members (Lombok accessors / loggers, MapStruct mappers, etc.) are not surfaced as inherited dependencies. The composite plan can propagate selected class-level annotations to the target through `propagate_class_annotations` — see `sm-refactor-java-extract-class` for the parameter's `auto` / `all` / `list:@Foo,@Bar` modes and the SLF4J `log`-field interaction.
- **`bbox_refactor_apply` refuses cross-worktree applies.** When the caller passes `cwd` (or the daemon's working directory resolves to a different git toplevel than the plan's recorded paths), apply bails with `error.cross_worktree_apply` rather than silently writing to the plan-time paths. Re-plan from the current worktree, or pass `force_path=true` to write to the plan's recorded paths anyway. When the caller omits `cwd` AND the daemon's `env::current_dir()` resolves outside any git tree (typical for systemd-managed daemons with `WorkingDirectory=$HOME`), the same refusal fires — pass `cwd` or `project_dir` from the dispatching tool to identify the caller's worktree.
- `java_lsp_organize_imports` is strongest with `jdtls` installed and available
  in the system path. JDTLS is now run as a warm per-project session reused
  across calls, so cold-start cost is paid once per `(project_dir, java)`
  pair. Without a working JDTLS, the source-tree heuristic fallback is used —
  it now also handles inner-class references (`Outer.Inner`) by leaving them
  qualified rather than fabricating an import.
- `migrate_java_type_usages` uses structural heuristics to distinguish type-use from constructor/call positions; always compile-verify after migration.
- For rename, move type, or package changes beyond the supported plan kinds, use JDT/IDE tooling or compiler-verified manual edits.
