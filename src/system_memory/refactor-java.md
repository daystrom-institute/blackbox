# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Current Capability

Java has full inspect-and-extract support, plus composite class extraction,
field/constructor wiring, caller delegation, interface extraction, visibility
rewriting, type migration, import organization, and Lombok-ification of
hand-rolled boilerplate (POJO DOJO).

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: method extraction, composite class extraction, nested class extraction, field moves/adds, constructor creation, delegate-field wiring, caller delegation, interface extraction, visibility rewriting, implements clause injection, type-use migration, import organization, and `lombokify_java_class` (POJO boilerplate → Lombok annotations).
- Semantic rename: not supported natively by blackbox yet; use JDT, IntelliJ, Eclipse, or another Java language-server/refactoring workflow.
- Import/package repair: `java_lsp_organize_imports` prefers a warm
  per-project JDTLS session (lazy-spawned, reused across calls, idle-evicted
  by the daemon) and falls back to tree-sitter plus project type scanning
  when JDTLS is unavailable or returns no edits. The fallback also keeps
  inner-class references in qualified `Outer.Inner` form.

Tree-sitter language: `java`.

## What `extract_java_class` Already Does For You

Read this section before reporting "gaps" — these are the contracts the
composite extract plan currently enforces, automatically, with no opt-in
flags. If you see behavior that contradicts the list below, the bug is
real; if your concern is on the list, the planner already handles it.

- **Target package is filesystem-derived.** When the target path lives
  under a `src/{main,test}/java/` ancestor, the planner emits the
  matching package declaration on the target. No need to hand-write
  `target_prelude` for cross-directory targets. Explicit `target_prelude`
  > existing target file's `package` > path-derived > source-package
  fallback (only when target shares directory with source) > hard error.
- **Cross-package extracts widen visibility to `public`.** Moved methods,
  generated getters/setters, and moved constants all get `public` on
  cross-package targets. Same-package extracts use the `package` floor.
- **Source gains an `import <target-package>.<TargetClass>;`** when the
  resolved target package differs from the source's.
- **Static-final captures move WITH their cross-cluster references
  rewritten.** When a `private static final` capture is referenced from
  source-side code outside the extracted methods, the planner:
  - Widens its visibility on the target to the package / public floor.
  - Rewrites every surviving source-side reference to qualified
    `<TargetClass>.<CONST>` form (bare reads AND `df.format(...)`
    method-call receivers).
  - Under `deep_analysis: true`, populates a
    `remaining_source_constant_refs` preview report listing every
    surviving site with line/column/context — analogous to
    `remaining_source_accessors`.
- **Mutable-capture-with-write is refused upfront.** If an extracted
  method body writes to a non-final source field that isn't in
  `move_fields`, the planner returns
  `error.bad_input(code=mutable_capture_with_write)` listing the
  offending fields. Promoting them to a `final` ctor parameter would
  produce moved code that fails javac `cannot assign to final variable`;
  the operator must add the field to `move_fields` (which routes it
  through the generated-setter delegate path).
- **Nested-class names in `item_names` get a directed error.**
  `extract_java_class` only takes method names. Passing an inner-class
  name yields `error.bad_input(code=nested_class_in_item_names)` with
  a pointer to extract the inner class manually first.
- **`remaining_source_accessors` excludes accesses inside the methods
  being extracted in the same plan.** Reads/writes inside `item_names`
  bodies move with the methods; only accesses that genuinely survive on
  source are reported. The report is a clean pre-apply preview.
- **`rewrite_remaining_accessors` is on by default whenever
  `move_fields` is non-empty.** Pre-apply source-side reads/writes are
  rewritten through the delegate's generated getter/setter regardless
  of `deep_analysis`. Pass `rewrite_remaining_accessors: false` to opt
  out.
- **Delegate-wiring statement is placed after the latest source-ctor
  assignment to any captured field.** Avoids `might not have been
  initialized` on `final` ctor-param captures and avoids null capture
  on non-final ones. `this(...)` / `super(...)` chains are not followed
  — the wiring lands at top-of-body in that case and may need manual
  adjustment.
- **External-call FIXME resolutions now include "drop the call".** For
  void-returning external calls whose side effect doesn't apply to the
  target, the operator-facing FIXME lists "drop the call" alongside
  the three structural fixes (add to extracted set / callback interface
  / inject source instance).
- **Overloaded methods disambiguated by signature suffix.** `item_names`
  accepts entries like `"methodName(Type1, Type2)"` to pick one specific
  overload by parameter types. Bare `"methodName"` still works when the
  name is unique; ambiguous bare names refuse with
  `error.bad_input(code=method_overload_ambiguous)` and enumerate the
  available overloads. Mismatched signature suffix refuses with
  `error.bad_input(code=method_overload_no_match)`. Type-text comparison
  is whitespace-normalized and ignores `final` / annotation prefixes.
- **Default-method interfaces are recognized as satisfied.** The
  `implements`-completeness check (the one that may emit
  `// FIXME: target now implements I but does not satisfy method(s) <X>`)
  filters out interface methods marked `default`, `static`, or `private`
  — those have bodies on the interface itself and don't need explicit
  declarations on the implementer. Default-only interfaces produce no
  false-positive FIXME.
- **`callback_externals` threads external calls as functional-interface
  callbacks.** Pass `callback_externals=[methodName, ...]` to route a
  source-class method through the target as a `Runnable` (no-arg void),
  `Supplier<R>` (no-arg non-void), `Consumer<T>` (single-arg void), or
  `Function<T,R>` (single-arg non-void) field instead of leaving a FIXME
  marker. The planner adds the field + ctor param on the target,
  rewrites each call site in the extracted bodies to `field.run()` /
  `.get()` / `.accept(arg)` / `.apply(arg)`, appends `this::method` to
  the source-side wiring expression, and imports `java.util.function.*`
  as needed. Two-plus-arg methods refuse with
  `error.bad_input(code=callback_arity_unsupported)` — wrap them or
  extract a real callback interface.

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
| `inner_class_calls_outer_method` | Inner calls a source-class method. v1 does not thread outer-method calls; refactor (inline or accept a `Runnable` callback) before promoting. |
| `inner_class_multiple_ctors` | Inner has more than one constructor. Consolidate first. |
| `inner_class_this_chain_ctor` | Inner's ctor delegates via `this(...)`. Inline the delegation first. |
| `inner_class_referenced_as_type` | Inner is referenced outside `new <Inner>(...)` (variable decl, cast, method reference, `Outer.Inner` path). v1 only rewrites instantiations; handle other sites manually. |

Workflow after promotion: run `extract_java_class` on the outer cluster
as a separate call. The moved methods will reference the promoted class
via the source's new import.
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

Capture resolution rules (Gap 19): `captured_variables` only contains
identifiers that resolve to a direct `field_declaration` of the **outer
source class**. Method parameters, local variables, enhanced-for variables,
and inner-class fields are not captures — they either travel with the method
or live in a separate scope. Bare-name reads inside the method body are
shadowing-checked against enclosing locals/parameters; only `this.<name>`
accesses bypass shadowing. This stops false captures like a method parameter
named the same as an inner-class field from being promoted into a constructor
parameter on the target.

Each capture also carries two mutability indicators (Gap 21):
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
  `in_method`, and `context` (`"direct"` or `"lambda"` — Gap 14: lambdas
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

### Generated FIXME markers in extract_java_class targets

When `deep_analysis: true` is set on `extract_java_class`, the planner not only
returns the structured reports but also scaffolds **FIXME comment markers** in
the generated target file at every unresolved call site, so the operator can
grep the target for `// FIXME: external call` / `// FIXME: inherited call` /
`// FIXME: target now implements` rather than cross-referencing the JSON
report against line numbers by hand.

Marker formats (stable, greppable):

- External call (Gap 22):
  ```java
  // FIXME: external call `applyFilters` — unresolved on target. Source-class method.
  //   resolutions: add to extracted set, extract callback interface, or inject source instance.
  applyFilters();
  ```
  Inserted directly above each unqualified call site of a source-class method
  that is not in the extracted set. Multiple call sites for the same method
  each receive their own marker.
- Inherited class call (Gap 23, `source_kind: class`):
  ```java
  // FIXME: inherited call `applyFilters` — inherited from class BaseView on the source. Extracted target does not extend BaseView.
  //   resolutions: extend the same superclass, inject the dependency, or move the call back to the source.
  applyFilters();
  ```
  Superclass dependencies are NEVER auto-resolved with `extends`; the FIXME
  marker is the only output.
- Implements injection (Gap 23, `source_kind: interface`):
  - When all interface-declared methods are present in the extracted set, the
    target's class declaration is rewritten to `public class T implements I` and
    the interface's import is added if needed.
  - When the interface is referenced via inherited call but the extracted set
    does not satisfy every declared method, the implements clause is still
    added (so the operator sees the contract), with a FIXME above the
    declaration:
    ```java
    // FIXME: target now implements HasLogger but does not satisfy method(s) <getLogger>;
    // either also extract the listed method(s) or remove the implements clause.
    public class CompositionMeterGrid implements HasLogger { ... }
    ```

- Mutable capture (Gap 29):
  ```java
  // FIXME: mutable capture `isPlantSelected` (source field is non-final). Promoted to `final` constructor param — value snapshotted at construction.
  //   resolutions: use Supplier<Boolean>, shared holder, or keep on source and access via reference.
  private final boolean isPlantSelected;
  ```
  Inserted directly above each generated `private final <Type> <name>;`
  field on the target whose corresponding capture has `source_mutable: true`
  AND `source_static_final: false`. Static-final captures route through the
  Gap 20 constants path and never become constructor params, so they don't
  receive this FIXME. Primitive types in the resolution hint are boxed for
  `Supplier<…>` (e.g. `boolean` → `Supplier<Boolean>`).

FIXME markers are only inserted when `deep_analysis: true`. With the flag off
the report is empty and the target file is generated bare. The marker format
is intentionally stable so downstream tooling and reviewers can pattern-match
on `// FIXME: external call \``, `// FIXME: inherited call \``,
`// FIXME: target now implements`, and `// FIXME: mutable capture \``.

4. Extract a cohesive Java class in one plan:

```text
bbox_refactor_plan(
  kind="extract_java_class",
  source="src/main/java/com/example/DashboardView.java",
  target="src/main/java/com/example/DashboardPipelinePressureGrid.java",
  module_name="DashboardPipelinePressureGrid",
  item_names=["getPipelinePressuresGrid","refreshPipelinePressuresData"],
  move_fields=["pipelinePressureSettingsGrid","pipelinePressureDataProvider"],
  delegate_field="pipelinePressureGrid",
  project_dir="/absolute/project/root"
)
```

Use this when the normal extract-class handoff is clear: methods move to a
missing target type, named fields move with them, remaining captured source
fields become target constructor parameters, the source gets a delegate field
and constructor assignment, and source-local calls to moved methods are
rewritten through that delegate. The response includes `captured_variables` so
you can review the dependency boundary before applying.

**Delegate wiring placement.** The
`this.<delegate_field> = new <target_class>(...)` statement is inserted
into the source's first constructor. When every captured argument is
also a constructor parameter (in scope from the parameter list),
insertion happens at the top of the body. When any captured argument
refers to a field rather than a parameter (the constructor assigns
`this.field = param;` somewhere in its body and the wiring needs the
post-assignment value), insertion is deferred until immediately after
the latest such `this.field = …` / `field = …` statement. This avoids
reading the captured fields while they are still `null` —
`final`-field captures would otherwise hit `might not have been
initialized`; non-`final` captures would silently capture null. The
placement logic walks only top-level statements of the chosen
constructor; `this(...)` / `super(...)` chains are not followed, so a
delegating constructor whose target ctor assigns the captured fields
ends up with the wiring at top-of-body and may need manual adjustment.

`captured_variables` entries carry `source_static_final` and `source_mutable`
booleans alongside `name`, `kind`, `source_type`, and `source_visibility`.
- `source_static_final: true` means the field is `private static final` (or
  any other-visibility static-final). The composite plan moves these as
  constants — declaration plus initializer is preserved on the target and
  the source declaration is removed. They do **not** become constructor
  parameters and the source-side delegate call does not pass them. (Gap 20)
- `source_mutable: true` means the field is non-`final`. Promoting it to a
  `final` constructor parameter snapshots the value at construction time —
  flag it for review before applying. (Gap 21 — companion field; the
  composite plan still promotes mutable captures to constructor params, but
  the boolean lets the operator decide whether to refactor through a
  `Supplier` / holder / shared reference instead.) When `deep_analysis: true`,
  the planner ALSO scaffolds a `// FIXME: mutable capture …` comment block
  directly above the promoted field on the target file, so the warning is
  visible in the generated source rather than buried in JSON (Gap 29).

The composite plan also widens visibility on extracted methods so the
source-side delegate calls produced by `update_java_callers` compile. The
floor is `package` for same-package extractions and `public` when the
target ends up in a different package than the source. Methods already at
or above the floor (e.g. `public`, or `protected` in same-package mode)
are emitted unchanged.

**Target package resolution.** The package decision uses a hybrid
precedence so cross-directory targets land in the correct package:

1. Explicit `target_prelude` containing `package <foo>;`.
2. Existing target file's `package` declaration (only when the target
   file already exists).
3. Source-root-derived from `target_path`'s filesystem location — walks
   ancestors of `target_path` for a `src/{main,test}/java/` triple and
   uses the longest (nearest) match. Multi-module Gradle/Maven layouts
   resolve against the deepest matching root.
4. Source's package — only when `target_path` shares a directory with
   `source_path` (the legacy same-package extract path).
5. Hard error — pass `target_prelude` with an explicit
   `package <foo>;` when no rule above resolves.

The cross-package detector compares the resolved target package against
the source's. When they differ, the planner:

- Sets the visibility floor for extracted methods to `public` (instead of
  `package`) so source-side delegate calls compile from a different
  package.
- Emits an additional `import <target-package>.<TargetClass>;` edit on
  the source so the new delegate field type resolves.

Same-package targets get the `package` floor and resolve the delegate
type implicitly. An explicit `visibility` parameter on the plan acts as
an additional floor — the planner widens further if you ask for `public`,
but never narrows below the cross-package requirement.

Pass `deep_analysis: true` to also receive:

- `external_calls` and `inherited_dependencies` (same shape as in step 3
  above), plus
- `remaining_source_accessors` — populated identically to
  `move_java_field`'s `deep_analysis` output (gap 26). When `move_fields` is
  non-empty AND `deep_analysis: true`, the plan response lists every read or
  write of each moved field that still lives in the source class after the
  declaration is removed. Empty `accesses` for a field means the move is
  clean; non-empty `accesses` flag the lines that will fail to compile after
  apply. The shape is documented under `move_java_field` in step 7 below;
  the contract is the same.

Whenever `move_fields` is non-empty, the planner **rewrites every
remaining source-side read/write through the delegate** and **generates
matching getter/setter declarations on the target**. The rewrite needs
only "this field was moved" — not the full call-graph walk that
`deep_analysis` triggers — so it runs by default regardless of
`deep_analysis`. Pass `rewrite_remaining_accessors: false` to opt out
(useful when the operator plans to hand-rewrite the remaining accesses
or has a custom delegation shape in mind). Behavior matrix:

| `deep_analysis` | `rewrite_remaining_accessors` | Behavior                                  |
|-----------------|-------------------------------|-------------------------------------------|
| `true`          | unset / `true`                | Rewrite reads/writes + emit accessors + populate `remaining_source_accessors` report |
| `true`          | `false`                       | Skip rewrites; report still populates     |
| `false`         | unset / `true`                | Rewrite reads/writes + emit accessors (no report)  |
| `false`         | `false`                       | Skip rewrites; no report — operator owns the leftover accesses |

The `remaining_source_accessors` report (read/write scan results) is
gated on `deep_analysis: true` because it walks every identifier in the
source body. The rewrite is not — it only inspects accesses to the
specific moved fields.

Rewrite shape:

| Access kind                    | Before                  | After                                            |
|--------------------------------|-------------------------|--------------------------------------------------|
| Bare read                      | `meterGrid`             | `delegate.getMeterGrid()`                        |
| `this.`-qualified read         | `this.meterGrid`        | `delegate.getMeterGrid()`                        |
| Method-on-field receiver       | `meterGrid.refresh()`   | `delegate.getMeterGrid().refresh()`              |
| Direct write                   | `items = list`          | `delegate.setItems(list)`                        |
| LHS-write whose RHS reads field| `items = items.stream()…`| `delegate.setItems(delegate.getItems().stream()…)`|
| Compound write (`+=`, `<<=`…)  | `counter += 5`          | `delegate.setCounter(delegate.getCounter() + 5)` |
| Increment / decrement          | `counter++`             | `delegate.setCounter(delegate.getCounter() + 1)` |

**LHS-write rewrite.** When a moved field appears on both sides of an
assignment (`field = field.transform()`), the planner emits a SINGLE
edit spanning the whole `assignment_expression` and replaces it with
`delegate.setField(<read-rewritten rhs>)`. RHS reads still route through
the getter — they live INSIDE the setter argument. Implementation: a
two-pass walk identifies LHS-write sites, then collects RHS sub-edits
per site and folds them into the combined write rewrite.

Caller-rewrites that fall inside an LHS-write RHS are absorbed too.
`update_java_callers` emits zero-width inserts at the start of moved
`method_invocation` nodes (e.g. `delegate.` before `buildGrid()`). When
the LHS-write RHS contains a call to a moved method
(`grid = buildGrid();`), the caller-rewrite is threaded through the
accessor-rewrite pass and folded into the setter argument
(`delegate.setGrid(delegate.buildGrid())`). The global edit list never
sees the absorbed caller edits, avoiding the overlap that would
otherwise trip the planner's non-overlap validator. A post-pass
containment check bails with `RefactorError` if a non-rendering edit
survives inside an LHS-write span — defense for future caller-rewrite
shapes that might slip through.

Generated accessors honour the same package/public visibility floor used
for moved methods (`package` same-package, `public` cross-package).
Boolean fields named `is*` / `has*` keep the bare name as their getter
(`isPlantSelected()`, not `getIsPlantSelected()`); `final` fields get a
getter only and writes against them are NOT rewritten — the original
write stays in place so the compiler surfaces the immutability error,
and the operator can decide whether to drop `final` or restructure.

Limitation: the rewrite walks tree-sitter alone and cannot detect the
narrow case where the field's type does not support the augmented
operator (`&=` on a non-numeric type, etc.). It still emits the rewrite
in setter form and lets the Java compiler complain.

The composite plan also runs the tree-sitter `organize_imports` heuristic
on the generated target file in-process before returning (gap 25). The
target's import block ends up containing only imports whose simple name
is referenced in the extracted method bodies; project-local types
referenced by simple name get a fresh import added when the type index
can resolve them uniquely. `import static …` and wildcard imports are
kept verbatim.

The reference walker recognizes type names in four syntactic positions:

- `type_identifier` nodes — variable declarations, return types, field
  types, generic bounds, etc.
- Uppercase-initial `identifier` used as the receiver (`object` field)
  of a `method_invocation` — captures static method calls like
  `DateUtils.parse(...)`, `Collectors.toList()`, `Math.abs(...)`.
- Uppercase-initial `identifier` used as the receiver of a
  `field_access` — captures static member references like
  `BigDecimal.ZERO`, `Optional.empty`-as-receiver patterns, enum value
  reads.
- Uppercase-initial `identifier` used as the qualifier of a
  `method_reference` (`Foo::bar` syntax) — captures method references
  like `FormCategoryEnumConverter::toLabel` that drop the implicit
  type qualifier onto the enclosing call.

The uppercase-initial check is convention-based; lower-case identifiers
in receiver position are treated as values, not types. False positives
(an uppercase-initial local variable that violates convention) resolve
as "no matching type" in the project index and get silently dropped.

This means the operator no longer needs a follow-up
`java_lsp_organize_imports` call solely to prune Vaadin-`@Route` or
CSV-writer-style noise, or to retain imports for types only referenced
as static-call receivers — though running JDTLS-backed
`organize_imports` afterward is still a good idea for full semantic
verification (third-party FQCN inference for types that aren't in the
project type index is out of scope for the heuristic).

**Wildcard coverage (Gap 28).** The same heuristic also drops explicit
single-type imports already covered by a wildcard from the same package.
Rule: after computing the final import set, group existing wildcard
imports (`import x.y.z.*;`) by package, then drop any explicit
`import x.y.z.SomeType;` whose package is `x.y.z`. `import static …` is
NEVER dropped — type wildcards do not cover static members. Explicit
imports from packages without a matching wildcard are also preserved.

This is structural, not semantic: it does not reason about overloads, static
context, inherited members, or framework injection. After applying it, run
the project compile/test command.

5. Add fields to the extracted class:

```text
bbox_refactor_plan(
  kind="add_java_fields",
  source="src/main/java/com/example/ExtractedMethods.java",
  fields=[
    {"visibility":"private","final":true,"type":"PlantPipelinePressureAdmin","name":"plantPipelinePressureAdmin"},
    {"visibility":"private","type":"Grid<PipelinePressureSettingsPlusData>","name":"pipelinePressureSettingsGrid"}
  ],
  project_dir="/absolute/project/root"
)
```

6. Add a constructor:

```text
bbox_refactor_plan(
  kind="add_java_constructor",
  source="src/main/java/com/example/ExtractedMethods.java",
  visibility="public",
  parameters=[
    {"type":"PlantPipelinePressureAdmin","name":"plantPipelinePressureAdmin"},
    {"type":"Provider<SessionData>","name":"sessionDataProvider"}
  ],
  assign_to_fields=true,
  project_dir="/absolute/project/root"
)
```

7. Move fields that belong with the extracted methods:

```text
bbox_refactor_plan(
  kind="move_java_field",
  source="src/main/java/com/example/DashboardView.java",
  target="src/main/java/com/example/ExtractedMethods.java",
  item_names=["pipelinePressureSettingsGrid","pipelinePressureDataProvider"],
  project_dir="/absolute/project/root"
)
```

Pass `deep_analysis: true` on the plan call to receive
`remaining_source_accessors`: for each moved field, every read/write of that
field that still lives in the source class after the declaration is removed.
The flag is opt-in (default `false`) because the scan walks every identifier
in the source body; on small classes the cost is negligible but on large
classes it's worth skipping when the operator knows the field is unique to
the moved cluster. Each entry carries `line`, `column` (1-indexed),
`kind` (`read` or `write`), and a trimmed `context` snippet of the line.
Empty `accesses` for a field means the source class no longer references it
and the move is clean. Non-empty `accesses` flag the lines that will fail to
compile until you rewrite them (commonly via `update_java_callers` with a
delegate or by moving more code along with the field). Shape:

```json
{
  "remaining_source_accessors": [
    {
      "field": "meterGrid",
      "accesses": [
        {"line": 270, "column": 36, "kind": "read",  "context": "viewContent.remove(meterGrid);"},
        {"line": 322, "column": 8,  "kind": "write", "context": "meterGrid = newGrid;"}
      ]
    }
  ]
}
```

Local variables and formal parameters that shadow the field name are
correctly skipped; bare `meterGrid` and `this.meterGrid` are both reported
when they resolve to the moved field.

7b. Move static final constants that travel with the extracted methods:

```text
bbox_refactor_plan(
  kind="move_java_constant",
  source="src/main/java/com/example/CompositionView.java",
  target="src/main/java/com/example/CompositionMeterGrid.java",
  item_names=["SAMPLE_STATUS_OK","SAMPLE_STATUS_NOT_OK","SAMPLE_STATUS_NO_DATASOURCE"],
  visibility="private",
  keep_copy=false,
  project_dir="/absolute/project/root"
)
```

This plan kind operates only on `field_declaration` nodes that have **both**
`static` **and** `final` modifiers. It removes the matched declarations from
the source and inserts them in the target with the configured `visibility`
(`private`, `package`, `protected`, or `public`), preserving the type, name,
and initializer verbatim. If `keep_copy=true`, the declarations stay in the
source and the source-side visibility is widened to at least `package` so
remaining source-class code can still see them; the target copy uses the
`visibility` you passed. The target file may be missing — it is created with
a `public class` wrapper using `module_name` (or the target file stem).

Use this when constants are referenced exclusively by methods that just moved
to another class. For instance fields, use `move_java_field` instead.

8. Add a delegate field to the original class and wire the first constructor:

```text
bbox_refactor_plan(
  kind="add_java_delegate_field",
  source="src/main/java/com/example/DashboardView.java",
  delegate_field="pipelinePressureGrid",
  delegate_type="DashboardPipelinePressureGrid",
  parameters=[
    {"type":"PlantPipelinePressureAdmin","name":"plantPipelinePressureAdmin"},
    {"type":"ProcessDataAdmin","name":"processDataAdmin"}
  ],
  project_dir="/absolute/project/root"
)
```

This adds `private final <delegate_type> <delegate_field>;` and inserts
`this.<delegate_field> = new <delegate_type>(...)` in the first constructor. If
the class has no constructor, it creates one.

9. Rewrite source-class call sites to delegate:

```text
bbox_refactor_plan(
  kind="update_java_callers",
  source="src/main/java/com/example/DashboardView.java",
  delegate_field="pipelinePressureGrid",
  item_names=["getPipelinePressuresGrid","refreshPipelinePressuresData"],
  project_dir="/absolute/project/root"
)
```

This rewrites unqualified calls such as `getPipelinePressuresGrid()` and
explicit `this.getPipelinePressuresGrid()` calls to
`pipelinePressureGrid.getPipelinePressuresGrid()`. It also rewrites
`this`-qualified Java method-reference syntax: `this::getPipelinePressuresGrid`
becomes `pipelinePressureGrid::getPipelinePressuresGrid`. Method references
qualified by a different receiver (e.g. `Foo::bar`, `super::bar`) are left
untouched, since they bind to a different instance and the rewrite would change
semantics.

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

14. Organize imports:

```text
bbox_refactor_plan(
  kind="java_lsp_organize_imports",
  source="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```

The planner asks JDTLS for workspace-aware organize-import edits through a
shared per-project session pool. The first call for a `(project_dir, java)`
pair lazily spawns JDTLS, awaits a real `initialize` response (no fixed
sleep), and sends `initialized`; subsequent calls reuse the same long-lived
child. Idle sessions are evicted on a 60s tick after `BLACKBOX_LSP_IDLE_SECS`
(default 600) of inactivity, and the daemon shuts every session down on stop.
Tunables: `BLACKBOX_JDTLS_INIT_TIMEOUT_SECS` (default 60) for the cold-start
window, `BLACKBOX_JDTLS_TIMEOUT_SECS` (default 30) per request,
`BLACKBOX_JDTLS_BIN` to point at a non-default binary. If JDTLS is absent, the
session is broken, or the request returns no edits, the tool falls back to a
structural project scan: removes plain imports whose simple names are no
longer referenced, keeps static and wildcard imports, and adds imports for
uniquely named Java source files in the same `project_dir` when their simple
type name is referenced. The fallback also detects inner-class-only simple
names and skips synthesizing imports for them — references like
`Outer.Inner` keep their qualified form rather than producing a non-resolving
`import x.Inner;`. It is not a full classpath resolver.

14b. Lombokify hand-rolled POJO boilerplate (POJO DOJO):

```text
bbox_refactor_plan(
  kind="lombokify_java_class",
  source="src/main/java/com/example/Pair.java",
  project_dir="/absolute/project/root"
)
```

Or against a directory tree (bulk mode — recommended for modernize/strip
runs against legacy POJO-heavy codebases):

```text
bbox_refactor_plan(
  kind="lombokify_java_class",
  source="src/main/java",
  project_dir="/absolute/project/root"
)
```

The planner detects six categories of canonical boilerplate and replaces
each with a semantically equivalent Lombok annotation:

| Hand-rolled shape | Replacement |
|-------------------|-------------|
| Trivial getter (`return field;` or `return this.field;`, public, no params, return-type matches field) | `@Getter` (class-level when every instance field qualifies; else per-field) |
| Trivial setter (`this.field = arg;`, public void, single param of matching type, field non-final) | `@Setter` (class-level when every non-final field qualifies; else per-field) |
| Apache Commons `EqualsBuilder` equals + `HashCodeBuilder` hashCode (BOTH must match the full instance-field set in declaration order; subset coverage refused) | `@EqualsAndHashCode` |
| Apache Commons `ToStringBuilder` toString (full-set coverage required) | `@ToString` |
| Canonical no-arg / all-args / required-args constructor (params match field set in declaration order, body is `this.field_i = param_i;` per field, public, no validation) | `@NoArgsConstructor` / `@AllArgsConstructor` / `@RequiredArgsConstructor` |
| `private static final Logger log = LoggerFactory.getLogger(<ThisClass>.class);` (field name MUST be exactly `log`) | `@Slf4j` |

**Collapsing.** When the full mutable-POJO set fires (class-level
`@Getter` + class-level `@Setter` + `@EqualsAndHashCode` + `@ToString` +
matching `@RequiredArgsConstructor` or `@NoArgsConstructor` on a
no-final-fields class), the five annotations collapse to a single
`@Data`. When every field is final, `@Getter` is class-level, no setters
are emitted, equals/hashCode/toString match, and `@AllArgsConstructor`
fires (== `@RequiredArgsConstructor` on all-final), the planner collapses
to `@Value` (Lombok's immutable variant). `@AllArgsConstructor` stacks
on top of `@Data` when both apply. `@Slf4j` stacks independently.

**Conservative refusal rules.** The detector errs toward leaving code
alone whenever Lombok-generated semantics would differ from the
hand-rolled method:

- Javadoc above a getter/setter/ctor disqualifies it (we don't silently
  drop documented method contracts).
- Setter with validation, normalization, or a fluent (`return this`)
  return is preserved.
- Fields with non-trivial getters (lazy init, null-coalescing, caching)
  are preserved per-field; class-level `@Getter` is then NOT emitted
  (would generate a duplicate-method compile error against the
  hand-rolled accessor).
- equals/hashCode that reference only a subset of fields is preserved
  (Lombok's default would change equality semantics).
- Unpaired equals OR hashCode is preserved (Lombok generates BOTH;
  dropping one would leave a phantom-paired method).
- Constructor with non-canonical body (validation, defaulting,
  reordering) is preserved.
- Multiple ctors classifying as the same Lombok kind (collision) →
  refuse all ctor lombokification rather than risk dropping the wrong
  one.
- SLF4J detection requires field name `log` exactly. `logger` /
  `LOG` / topic-named loggers are preserved.

**Format-difference caveat.** Apache `ToStringBuilder` default style
emits `Foo@hash[field=value, ...]`; Lombok `@ToString` emits
`Foo(field=value, ...)`. equals/hashCode value parity is preserved
(matching field set + matching order); toString output FORMAT changes.
Callers that depend on a specific toString format should opt out.

**Boolean-getter API safety (Gap 1).** When a primitive `boolean`
field has a hand-rolled `getXxx()` getter, dropping it would silently
break callers because Lombok's `@Getter` generates `isXxx()`. The
default `boolean_getter_strategy: "skip"` preserves the original
getter and falls back to per-field placement on the rest of the
class. Pass `boolean_getter_strategy: "bridge"` to drop the original
and emit a one-line bridge `public boolean getXxx() { return isXxx(); }`
so callers continue to compile alongside Lombok's generated form.
Pass `"rename"` to drop without a bridge — only when callers don't
exist or are being rewritten in the same pass. Symmetric for boxed
`Boolean` with `is-`prefix getters (Lombok generates `getXxx()` for
boxed types).

**Plan-to-file (Gap 3 + response-size).** Pass
`output_path: "<filepath>"` to write the full RefactorPlan JSON to
disk and receive a compact `RefactorPlanSummary` instead of the full
plan body inline. Required for large refactors whose plan JSON
exceeds the MCP transport's parameter-string limit (e.g., a class
with hundreds of trivial accessors). Apply the saved plan via
`bbox_refactor_apply(plan_path="<filepath>", confirm=true)` — the
apply path reads from disk and runs the same transactional pipeline
as inline plans. Summary contains: `plan_path`, kind, file/edit
counts, per-file (`path`, `edit_count`, `original_sha256`), and the
full `leftovers` list (already small).

**Bulk mode.** When `source` resolves to a directory, the planner
walks every `.java` file beneath it (skipping `target/`, `build/`,
`out/`, hidden dirs), runs the single-file lombokifier per class, and
aggregates per-file FileEdits into one composite plan. Files that
refuse for any reason (no boilerplate, parse failure, validation-bearing
ctor, etc.) appear in `plan.leftovers` as `<path>: <reason>` entries
the operator can audit. The composite plan inherits the existing
`bbox_refactor_apply` transactional semantics: any per-file
parse-validation failure rolls the entire batch back.

**Prerequisites.** Lombok must already be on the project's classpath
(`compileOnly 'org.projectlombok:lombok'` + `annotationProcessor
'org.projectlombok:lombok'` for Gradle, or the equivalent Maven
dependency). The lombokifier does NOT add Lombok to the build — that
is a separate one-time step the operator owns. Apache Commons Lang3
imports (`EqualsBuilder`, `HashCodeBuilder`, `ToStringBuilder`)
become unused after lombokification; run a follow-up
`java_lsp_organize_imports` pass to prune them.

**Class targeting.** In single-file mode the planner picks the first
top-level class declaration unless `item_names=[<class>]` overrides it.
In bulk mode `item_names` is ignored — every file targets its first
top-level class (the standard `Foo.java` contains class `Foo`
convention). Inner classes are not converted in bulk mode; invoke
single-file mode with `item_names=[<inner>]` if you need that.

14c. Curated-batch lombokification with per-step skip:

```text
bbox_refactor_run(
  title="lombokify curated batch",
  project_dir="/absolute/project/root",
  confirm=true,
  steps=[
    {"op":"plan","kind":"lombokify_java_class","source":"src/.../A.java","optional":true},
    {"op":"plan","kind":"lombokify_java_class","source":"src/.../B.java","optional":true},
    {"op":"plan","kind":"lombokify_java_class","source":"src/.../C.java","optional":true},
    {"op":"command","command":"./gradlew","args":["compileJava"]}
  ]
)
```

`optional: true` on plan steps converts plan-time failures (e.g., "no
lombokifiable boilerplate") into per-step `skipped` entries in the run
report rather than aborting the whole batch (Gap 2 from
JAVA_TOOL_GAPS — a single non-POJO file in a 7-file batch was rolling
back 4 successfully-written prior steps). Default `optional: false`
preserves strict batch semantics for refactors where every step must
succeed.

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
- **Annotation-processor-generated members are invisible to dependency analysis.** The `inherited_dependencies` walk traverses `extends` / `implements` chains in the project type index — it does not run annotation processors. Class-level annotations that generate members (Lombok `@Slf4j` → `log`, `@Data` / `@Getter` / `@Setter` → accessors, MapStruct-generated mappers, etc.) are not surfaced as inherited dependencies. If the extracted method body references such a generated member (`log.info(...)` from `@Slf4j` on the source class), the extracted target must either re-declare the same class-level annotation or accept the member as a constructor-injected dependency. The composite plan does not propagate class-level annotations between source and target.
- `java_lsp_organize_imports` is strongest with `jdtls` installed and available
  in the system path. JDTLS is now run as a warm per-project session reused
  across calls, so cold-start cost is paid once per `(project_dir, java)`
  pair. Without a working JDTLS, the source-tree heuristic fallback is used —
  it now also handles inner-class references (`Outer.Inner`) by leaving them
  qualified rather than fabricating an import.
- `migrate_java_type_usages` uses structural heuristics to distinguish type-use from constructor/call positions; always compile-verify after migration.
- For rename, move type, or package changes beyond the supported plan kinds, use JDT/IDE tooling or compiler-verified manual edits.
