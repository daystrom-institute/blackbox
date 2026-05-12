# Java Lombokify — Hand-Rolled POJO Boilerplate to Lombok Annotations

Use this memory when planning `lombokify_java_class` runs. The plan kind
converts canonical hand-rolled boilerplate (getters/setters, Apache
Commons equals/hashCode/toString, canonical constructors, SLF4J logger
fields) into the corresponding Lombok annotations, optionally collapsing
the full mutable-POJO set into `@Data` or the all-final-fields set into
`@Value`. Parent runbook: `sm-refactor-java` (general Java tool
sequence, capability matrix, agent atom catalog). For class-extraction
and capture analysis, see `sm-refactor-java-extract-class`.

## Minimal invocation skeleton

```text
bbox_refactor_status(project_dir="/absolute/project/root", supported_kinds=true)
bbox_refactor_plan(kind="lombokify_java_class", source=<file_or_dir>, project_dir=...)
bbox_refactor_apply(plan=<plan>, confirm=true)
# follow up with project compile/test:
./gradlew compileJava || ./mvnw compile
```

Bulk mode and curated-batch composition are detailed below.

## Single-file

```text
bbox_refactor_plan(
  kind="lombokify_java_class",
  source="src/main/java/com/example/Pair.java",
  project_dir="/absolute/project/root"
)
```

## Bulk mode (directory tree)

Recommended for modernize/strip runs against legacy POJO-heavy
codebases:

```text
bbox_refactor_plan(
  kind="lombokify_java_class",
  source="src/main/java",
  project_dir="/absolute/project/root"
)
```

## Boilerplate detection table

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

**Boolean-getter API safety.** When a primitive `boolean`
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

**Plan-to-file.** Pass `output_path: "<filepath>"` to write the full
RefactorPlan JSON to disk and receive a compact `RefactorPlanSummary`
instead of the full plan body inline. Required for large refactors
whose plan JSON exceeds the MCP transport's parameter-string limit
(e.g., a class with hundreds of trivial accessors). Apply the saved
plan via `bbox_refactor_apply(plan_path="<filepath>", confirm=true)` —
the apply path reads from disk and runs the same transactional pipeline
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

## Curated-batch lombokification with per-step skip

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
report rather than aborting the whole batch — a single non-POJO file in
a 7-file batch would otherwise roll back any successfully-written prior
steps. Default `optional: false` preserves strict batch semantics for
refactors where every step must succeed.
