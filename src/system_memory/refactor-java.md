# Java Refactor Mechanization Runbook

Use this memory before operating on Java files with blackbox refactor tools.

## Current Capability

Java is an inspect-first backend today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no Java-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use JDT, IntelliJ, Eclipse,
  or another Java language-server/refactoring workflow.
- Import/package repair: not automatic; use the project build tool and IDE/LSP.

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

2. Search and inspect neighbors:

```text
bbox_hybrid_search(
  query="class or method name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

3. Make edits with the normal code editing path, then validate with project
commands. Common commands:

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
- For rename, move type, extract interface, or package changes, use JDT/IDE
  tooling or compiler-verified manual edits.

