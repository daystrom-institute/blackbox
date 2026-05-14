+++
title = "Java extract_java_class — composite class extraction, capture analysis, FIXME catalog"
tags = ["refactor", "refactoring", "mechanization", "java", "extract", "extract_java_class", "extract_java_methods", "move_java_fields", "move_java_constant", "add_java_fields", "add_java_constructor", "add_java_delegate_field", "rewrite_java_calls_to_delegate", "tree-sitter", "bbox_refactor_plan", "bbox_refactor_apply", "capture-analysis", "captured_variables", "callback_externals", "wiring_mode", "source_delegate_wrappers", "propagate_class_annotations", "rewrite_remaining_accessors", "deep_analysis", "external_calls", "inherited_dependencies", "remaining_source_accessors", "remaining_source_constant_refs", "FIXME", "delegate", "validation_failed", "mutable_capture_with_write", "method_overload_ambiguous", "guice_field_inject"]
order = 13
template = false
+++
# Java `extract_java_class` — Composite Class Extraction

Use this memory when planning `extract_java_class` runs. The plan kind
extracts a cohesive method-and-field cluster from a source class into a
new target class, generating delegate-field wiring, caller delegation,
capture-aware constructor parameters, visibility widening, import
injection, and (under `deep_analysis: true`) a structured preview of
remaining-source accessors / external calls / inherited dependencies.

Parent runbook: `sm-refactor-java` (general Java tool sequence, capability
matrix, contextual atom signposts, interface/visibility/migration/imports
primitives). For Lombok conversion, see `sm-refactor-java-lombokify`.

## Minimal invocation skeleton

```text
bbox_refactor_status(project_dir=<root>, supported_kinds=true)
bbox_refactor_plan(
  kind="extract_java_class",
  source="src/main/java/com/example/Big.java",
  target="src/main/java/com/example/extracted/Small.java",
  module_name="Small",
  item_names=["doA", "doB"],
  move_fields=["sharedState"],
  deep_analysis=true,
  project_dir=<root>
)
bbox_refactor_apply(plan=<plan>, confirm=true)
./gradlew compileJava  # or ./mvnw compile
./gradlew test         # or ./mvnw test
```

For pre-extraction grounding (cluster discovery, public-API delta,
project-wide reference walk) prefer the refactor atoms catalogued in
`sm-refactor-java`: `java-class-dependency-graph`, `java-public-api-guard`,
`java-find-usages`, `java-extract-cohesive-class`. The reference below
documents the underlying primitive when the atoms don't fit your shape.

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
- **Public-static cross-class calls are auto-qualified.** When
  `deep_analysis: true` classifies an external_call as a `public static`
  method on the source class, the planner rewrites the unqualified call
  in the moved body to `<SourceClass>.<method>(...)` and **skips the
  FIXME marker**. The source-class import is added to cross-package
  targets. Detection is AST-driven (`method_invocation` nodes with no
  `object` field) — string literals and `// ...` comments containing
  the method name are skipped at parser level.
- **`external_calls` entries carry visibility + recommended resolution.**
  Each `ExternalCall` in the deep_analysis report has
  `source_visibility` (`public`/`protected`/`package`/`private`),
  `source_is_static` (bool), and `recommended_resolution`. Heuristic:
  public-static → `cross_class_static_call` (auto-resolved by the
  qualifier rewrite above); private → `add_to_item_names` (high
  confidence — privates have no out-of-class callers); other shapes
  leave `recommended_resolution` absent for operator decision.
- **Cross-package bare-field access on inner-type DTOs routes through
  getters.** When the moved methods take a parameter typed as a
  source-class inner type (`public static class Ticket` with
  `private final` fields + public `getX()` / `isX()` accessors), and
  the extract is cross-package, `param.field` accesses in the extracted
  body get rewritten to `param.getField()` / `param.isField()`. Lookup
  walks BOTH method parameters AND local-variable declarations whose
  explicit declared type matches (`Detail d = lookup(); d.field` works;
  `var d = lookup()` does not — type inference is out of scope). The
  rewrite only triggers on inner types declared inside the source class
  and only fires when the inner type has a matching public getter.
  Same-package extracts leave POJO bare access alone.
- **Java record components route through bare-name accessors.** When
  the moved methods access a component of a source-class `record` inner
  type (`param.componentName` or `localVar.componentName`), the planner
  rewrites to `param.componentName()` — records auto-generate private
  final backing fields and public accessors named after the component
  (no `get` prefix). Same receiver-discovery path as the POJO rewrite:
  method parameters AND local variables with explicit record-type
  declarations both get rewritten. Triggers SAME-package too — record
  private fields aren't package-accessible, so the rewrite is needed
  for any extract out of the declaring class. POJO inner-type rewriting
  stays cross-package gated; record-component rewriting is
  unconditional.
- **Source-class inner type references are qualified + widened.** When
  extracted bodies reference an inner type (enum, class, record, or
  interface) declared inside the source class — bare `InnerType`,
  `InnerType.VALUE`, `InnerType.staticCall()`, OR method-reference
  qualifier (`InnerType::new`, `InnerType::method`) — the planner
  rewrites each reference to `<SourceClass>.<InnerType>` on the target,
  adds an `import <source-package>.<SourceClass>;` to cross-package
  targets, and widens the inner type's source-side visibility to the
  same floor used for moved methods (`package` same-pkg, `public`
  cross-pkg). Inner types already at/above the floor stay unchanged.
  References the operator already qualified as `Outer.Inner` are
  left alone.
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
- **Method-reference field qualifiers are captured.** A reference like
  `csvExtractors::extractC4Composition` inside an extracted method body
  where `csvExtractors` is an instance field of the source class now
  threads `csvExtractors` through the target's constructor. Pre-fix the
  qualifier was silently dropped from capture analysis, producing
  `error: cannot find symbol` at every `::` site after apply.
- **Cross-file static caller rewrite.** When the move is cross-package
  and the moved item is `static` (method or `static final` constant),
  the planner walks every `.java` file under `project_dir`, finds
  `OldOwnerClass.<symbol>` references — method invocations,
  `OldOwnerClass.CONST` field accesses, `OldOwnerClass::method`
  references — and rewrites the qualifier to `NewOwnerClass`. Cross-
  package callers also get `import <target-pkg>.<NewOwnerClass>;`
  injected. Each touched file shows up in `plan.edits` and is
  individually parse-validated. Build dirs (`target/`, `build/`,
  `.gradle/`, `node_modules/`, `.git/`) are skipped. Instance method
  callers are NOT rewritten — the source-side delegate field is
  private and unreachable from other files; operator handles those.
- **Stacked-extract ctor wiring ordering conflict diagnosed.** When the
  accessor-rewriter rewrites an existing `this.X = new ...(field)`
  line to read `binder.getField()` AND the new `this.binder = new ...`
  wiring lands at a later byte position in the same ctor (because
  field-only captures pushed it down), the planner emits a
  `tracing::warn` with `code=ctor_wiring_ordering_conflict`, the
  delegate field name, and both line numbers. Apply still produces a
  file — but operator sees the warning in the daemon log and must swap
  the two statements manually. There's no safe auto-fix: pulling the
  wiring back past its field-only-capture lower bound would silently
  null-capture those fields. Future work may relocate the field-only
  assignments above the conflicting accessor rewrite.
- **`validation_failed` returns excerpt windows.** When the planned
  post-edit source has a parse error, the `validations` array now
  carries an `error_excerpts` field with `{line, column, byte_start,
  byte_end, snippet}` for the first 5 ERROR / MISSING nodes. The
  snippet is a 3-line window with a line-number gutter. Operators no
  longer need to re-run the plan, pipe to a file, or pivot to a
  different class just to locate where the rewrite broke parse.

## Optional parameters on `extract_java_class`

- **`wiring_mode`** — how the source class wires the new delegate field.
  Values:
  - `constructor_args` (default for plain-Java classes): `private final
    <Target> <delegate>;` + `this.<delegate> = new <Target>(...)` in
    the source's first constructor.
  - `guice_field_inject`: emit `@Inject private <Target> <delegate>;`
    on the source, skip ctor wiring entirely. The DI container
    populates the delegate after construction. The target's
    constructor also gets `@Inject` and `import javax.inject.Inject;`
    so DI can construct it with its captured ctor params.
  - `manual`: skip source-side wiring (no delegate field, no ctor
    assignment). Operator wires in their own code.

  **Auto-detect refusal.** When `wiring_mode` is unset AND the source
  class has any `@Inject`-annotated field, the planner refuses with
  `error.bad_input(code=guice_field_injection_detected)` — the
  default `constructor_args` flow would capture null because injection
  happens AFTER the constructor. Pass `wiring_mode` explicitly to
  proceed.

  **Import dedupe.** `@Inject` lives in two packages (`com.google.inject`
  and `javax.inject`). The import-injection paths skip when the source
  already imports either FQCN (same simple name) AND when a wildcard
  import covers the SAME package as the new import (in which case the
  explicit form is redundant). Foreign wildcards from unrelated packages
  do NOT block import addition — the previous blanket skip silently
  dropped legitimate imports on any source carrying `import java.util.*;`
  or similar.

  **Guice mutable-capture suppression.** Under
  `wiring_mode=guice_field_inject` the target also `@Inject`-constructs,
  so its captured ctor params are freshly injected at construction time
  and do NOT carry a stale snapshot of the source field. The
  "mutable capture promoted to final ctor param" FIXME is
  suppressed in that mode — it warns about a failure mode that doesn't
  apply.

- **`source_delegate_wrappers`** — when `true`, generate thin wrapper
  methods on the source for each moved public non-static method. Each
  wrapper has the original method's signature (parameters + `throws`
  clause preserved) and a body that delegates to the new field:
  `return <delegate>.<method>(args);` (or bare call for void returns).
  Cross-file callers holding references to the source class continue
  to compile against the wrapper. Default `false` (preserves the
  current breaking-extract behavior). Static methods are NOT wrapped
  — they're handled by the public-static auto-qualify rewrite.

- **`propagate_class_annotations`** — controls whether class-level
  annotations on the source class propagate to the target. Values:
  - `auto` (default): scan moved bodies for identifiers generated by
    known annotation processors. Current support covers `@Slf4j` → `log` (with
    `import lombok.extern.slf4j.Slf4j;`).
  - `all`: copy every class-level annotation verbatim, no detection.
  - `none`: strip everything (the legacy behavior).
  - `list:@A,@B`: operator-supplied allowlist.

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

The walker also visits `marker_annotation` and `annotation` nodes,
capturing the annotation's simple name (`@Nullable`, `@Transactional`,
custom annotations, plus the qualified-name form `@some.pkg.Annot`).
JDK annotation builtins (`@Override`, `@Deprecated`, `@SuppressWarnings`,
`@FunctionalInterface`, `@SafeVarargs`) are filtered out; everything
else routes through the project type index for import preservation.
`annotated_type` nodes (`@Nullable String foo`) parse with the
annotation as a child, so they are covered too.

This means the operator no longer needs a follow-up
`java_lsp_organize_imports` call solely to prune Vaadin-`@Route` or
CSV-writer-style noise, or to retain imports for types only referenced
as static-call receivers — though running JDTLS-backed
`organize_imports` afterward is still a good idea for full semantic
verification (third-party FQCN inference for types that aren't in the
project type index is out of scope for the heuristic).

**Wildcard coverage.** The same heuristic also drops explicit
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

## Generated FIXME markers in extract_java_class targets

When `deep_analysis: true` is set on `extract_java_class`, the planner not only
returns the structured reports but also scaffolds **FIXME comment markers** in
the generated target file at every unresolved call site, so the operator can
grep the target for `// FIXME: external call` / `// FIXME: inherited call` /
`// FIXME: target now implements` rather than cross-referencing the JSON
report against line numbers by hand.

Marker formats (stable, greppable):

- External call:
  ```java
  // FIXME: external call `applyFilters` — unresolved on target. Source-class method.
  //   resolutions: add to extracted set, extract callback interface, or inject source instance.
  applyFilters();
  ```
  Inserted directly above each unqualified call site of a source-class method
  that is not in the extracted set. Multiple call sites for the same method
  each receive their own marker.
- Inherited class call (`source_kind: class`):
  ```java
  // FIXME: inherited call `applyFilters` — inherited from class BaseView on the source. Extracted target does not extend BaseView.
  //   resolutions: extend the same superclass, inject the dependency, or move the call back to the source.
  applyFilters();
  ```
  Superclass dependencies are NEVER auto-resolved with `extends`; the FIXME
  marker is the only output.
- Implements injection (`source_kind: interface`):
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

- Mutable capture:
  ```java
  // FIXME: mutable capture `isPlantSelected` (source field is non-final). Promoted to `final` constructor param — value snapshotted at construction.
  //   resolutions: use Supplier<Boolean>, shared holder, or keep on source and access via reference.
  private final boolean isPlantSelected;
  ```
  Inserted directly above each generated `private final <Type> <name>;`
  field on the target whose corresponding capture has `source_mutable: true`
  AND `source_static_final: false`. Static-final captures route through the
  static-final constants path and never become constructor params, so they don't
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
  parameters and the source-side delegate call does not pass them.
- `source_mutable: true` means the field is non-`final`. Promoting it to a
  `final` constructor parameter snapshots the value at construction time —
  flag it for review before applying. The
  composite plan still promotes mutable captures to constructor params, but
  the boolean lets the operator decide whether to refactor through a
  `Supplier` / holder / shared reference instead.) When `deep_analysis: true`,
  the planner ALSO scaffolds a `// FIXME: mutable capture …` comment block
  directly above the promoted field on the target file, so the warning is
  visible in the generated source rather than buried in JSON.

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
