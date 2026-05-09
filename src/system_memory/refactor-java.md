# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Current Capability

Java has full inspect-and-extract support, plus interface extraction, visibility rewriting, type migration, and JDTLS integration for import organization.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: method extraction, nested class extraction, interface extraction, visibility rewriting, implements clause injection, type-use migration, and JDTLS-backed import organization.
- Semantic rename: not supported natively by blackbox yet; use JDT, IntelliJ, Eclipse, or another Java language-server/refactoring workflow.
- Import/package repair: automatic via JDTLS (`java_lsp_organize_imports`).

Tree-sitter language: `java`.

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds, names where tree-sitter exposes them, byte ranges, and line ranges.

2. Extract methods or nested classes:

```text
bbox_refactor_plan(
  kind="extract_java_methods",
  source="src/main/java/com/example/GodClass.java",
  target="src/main/java/com/example/ExtractedMethods.java",
  item_names=["myMethod1", "myMethod2"],
  project_dir="/absolute/project/root"
)
```

3. Extract an interface from a class:

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

4. Add `implements` clause to a class:

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

5. Rewrite method visibility:

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

6. Migrate type usages (concretion → interface):

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

7. Organize imports via JDTLS:

```text
bbox_refactor_plan(
  kind="java_lsp_organize_imports",
  source="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```

8. Compound run — full extract-interface flow with rollback:

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

9. Validate with project commands:

```text
mvn test
./mvnw test
gradle test
./gradlew test
```

## Safety Rules

- Do not apply Rust plan kinds to Java files.
- Tree-sitter does not enforce package/path consistency, generic type binding, annotation processing, Lombok/generated code, or classpath semantics.
- `jdtls` execution requires `jdtls` to be installed and available in the system path.
- `migrate_java_type_usages` uses structural heuristics to distinguish type-use from constructor/call positions; always compile-verify after migration.
- For rename, move type, or package changes beyond the supported plan kinds, use JDT/IDE tooling or compiler-verified manual edits.
