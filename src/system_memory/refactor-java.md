# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Current Capability

Java has full inspect-and-extract support, plus composite class extraction,
field/constructor wiring, caller delegation, interface extraction, visibility
rewriting, type migration, and import organization.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: method extraction, composite class extraction, nested class extraction, field moves/adds, constructor creation, delegate-field wiring, caller delegation, interface extraction, visibility rewriting, implements clause injection, type-use migration, and import organization.
- Semantic rename: not supported natively by blackbox yet; use JDT, IntelliJ, Eclipse, or another Java language-server/refactoring workflow.
- Import/package repair: `java_lsp_organize_imports` prefers a warm
  per-project JDTLS session (lazy-spawned, reused across calls, idle-evicted
  by the daemon) and falls back to tree-sitter plus project type scanning
  when JDTLS is unavailable or returns no edits. The fallback also keeps
  inner-class references in qualified `Outer.Inner` form.

Tree-sitter language: `java`.

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

This is structural, not semantic: it does not reason about overloads, static
context, visibility across packages, inherited members, or framework injection.
After applying it, run `java_lsp_organize_imports` and the project compile/test
command.

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
`pipelinePressureGrid.getPipelinePressuresGrid()`.

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
- `java_lsp_organize_imports` is strongest with `jdtls` installed and available
  in the system path. JDTLS is now run as a warm per-project session reused
  across calls, so cold-start cost is paid once per `(project_dir, java)`
  pair. Without a working JDTLS, the source-tree heuristic fallback is used —
  it now also handles inner-class references (`Outer.Inner`) by leaving them
  qualified rather than fabricating an import.
- `migrate_java_type_usages` uses structural heuristics to distinguish type-use from constructor/call positions; always compile-verify after migration.
- For rename, move type, or package changes beyond the supported plan kinds, use JDT/IDE tooling or compiler-verified manual edits.
