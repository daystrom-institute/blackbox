# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Current Capability

Java is an inspect-and-extract backend, with JDTLS integration for import organization.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: supports method and nested class extraction, plus JDTLS-backed import organization.
- Semantic rename: not supported natively by blackbox yet; use JDT, IntelliJ, Eclipse,
  or another Java language-server/refactoring workflow.
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

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this to
map packages, imports, classes, interfaces, enums, records, and candidate move
ranges.

2. Extract methods or nested classes:

To extract one or more methods into a target file:
```text
bbox_refactor_plan(
  kind="extract_java_methods",
  source="src/main/java/com/example/GodClass.java",
  target="src/main/java/com/example/ExtractedMethods.java",
  item_names=["myMethod1", "myMethod2"],
  project_dir="/absolute/project/root"
)
```

To extract nested classes:
```text
bbox_refactor_plan(
  kind="extract_java_nested_classes",
  source="src/main/java/com/example/GodClass.java",
  target="src/main/java/com/example/ExtractedClass.java",
  item_names=["NestedDto"],
  project_dir="/absolute/project/root"
)
```

3. Organize imports via JDTLS:

After extraction, you can organize imports in the source or target file using the JDTLS-backed planner:
```text
bbox_refactor_plan(
  kind="java_lsp_organize_imports",
  source="src/main/java/com/example/Thing.java",
  project_dir="/absolute/project/root"
)
```
Then use `bbox_refactor_apply` as usual.

4. Validate with project commands:

```text
mvn test
./mvnw test
gradle test
./gradlew test
```

Use the wrapper and targets actually present in the repository.

## Safety Rules

- Do not apply Rust plan kinds to Java files.
- Tree-sitter does not enforce package/path consistency, generic type binding,
  annotation processing, Lombok/generated code, or classpath semantics.
- `jdtls` execution requires `jdtls` to be installed and available in the system path. It runs locally and may take a few seconds to boot per organization command.
- For rename, move type, extract interface, or package changes beyond simple extraction and import management, use JDT/IDE tooling or compiler-verified manual edits.
