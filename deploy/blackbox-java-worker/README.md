# Blackbox Java Worker

JSON-RPC 2.0 stdio sidecar for Java source synthesis using [OpenRewrite](https://github.com/openrewrite/rewrite).
Mirrors the architecture of `deploy/blackbox-csharp-worker/`.

## Critical Invariant

**This process is filesystem-free.** All source content arrives via request
params; all output goes via JSON response. The Rust client owns file I/O and
transaction management.

## Runtime Requirement: JDK 21

**The JAR must be launched with a JDK 21 `java` binary.** It can be *built*
with any JDK ≥ 21 (the Maven compiler target is 21), but at *runtime* OpenRewrite's
bundled `rewrite-java-21` parser uses JDK-internal `com.sun.tools.javac` APIs
that are only present in the JDK they target. Running on JDK 22+ produces a
`NoClassDefFoundError` on every parse call with no indication of the root cause.

Set `BLACKBOX_JAVA_BIN` to your JDK 21 binary:

```bash
export BLACKBOX_JAVA_BIN=/usr/lib/jvm/java-21-openjdk/bin/java
```

The Rust client enforces this at startup: it calls `getCapabilities`, parses the
reported `java_version`, and fails closed with a clear error if the major version
is not 21. When OpenRewrite ships a `rewrite-java-NN` module and the dependency
is updated, update `REQUIRED_JAVA_MAJOR` in `src/macros/java_sidecar.rs` and the
`pom.xml` `rewrite-java-*` dependency together.

## Build

```bash
cd deploy/blackbox-java-worker
mvn package -DskipTests
```

Produces `target/blackbox-java-worker.jar` — a shaded (fat) executable jar.

## Run

```bash
java -jar target/blackbox-java-worker.jar
```

The process reads one JSON-RPC 2.0 request per line from stdin and writes one
response per line to stdout. Sends `{"ok": true}` on shutdown and exits 0.

## Environment

| Variable | Purpose |
|---|---|
| `BLACKBOX_JAVA_WORKER_JAR` | Path to the worker jar (set in Rust config) |

## Protocol

JSON-RPC 2.0 over stdin/stdout, one JSON object per line. All field names are
snake_case. Requests have `id`, `method`, `params`; responses have `id`,
`result` (on success), or `error` (with `code`/`message`/`data`).

### `getCapabilities`

Returns worker metadata and the set of supported operations.

**Params:** `{}`

**Result:**

```json
{
    "protocol_version": 1,
    "worker_version": "1.0.0",
    "java_version": "21.0.11",
    "openrewrite_version": "8.41.0",
    "supported_ops": ["emit_type", "insert_member"]
}
```

### `emitType`

Validate the macro-supplied source for a new top-level Java type and return a
file-create descriptor. The worker does **not** generate a skeleton — it parses
and validates the caller-provided `source_text`, confirms its declared package,
type name, and kind match the params, and returns the format-preserving
round-trip as the file content. Parse failure → `error.parse_invalid`; a
package/name/kind mismatch → `error.type_mismatch`.

**Params:**

```json
{
    "source_root": "/path/to/src/main/java",
    "package": "com.example",
    "name": "MyClass",
    "kind": "class",
    "source_text": "package com.example;\n\npublic class MyClass {\n}\n"
}
```

- `source_root` — base directory for source files (the file path is computed as
  `source_root/<package as dirs>/<name>.java`)
- `package` — Java package the source must declare
- `name` — top-level type name the source must declare (valid Java identifier)
- `kind` — one of `class`, `interface`, `enum`, `record`
- `source_text` — full source of the new type (required); validated, not generated

**Result:**

```json
{
    "file_creates": [
        {
            "path": "/path/to/src/main/java/com/example/MyClass.java",
            "content": "package com.example;\n\npublic class MyClass {\n\n}\n"
        }
    ],
    "diagnostics": []
}
```

**Errors:** `error.parse_invalid` if kind or name is invalid.

---

### `insertMember`

Insert a member declaration (field, method, constructor, inner type) into an
existing Java type in the provided source text.

**Params:**

```json
{
    "target_file": "/path/to/MyClass.java",
    "source_text": "package com.example;\n\npublic class MyClass {\n\n}",
    "target_type": "MyClass",
    "member_text": "public void hello() {\n    System.out.println(\"Hi\");\n}",
    "imports": []
}
```

- `target_file` — file path (for reference only; not read)
- `source_text` — full file content
- `target_type` — simple name of the class/interface/enum to modify
- `member_text` — source code of the member to insert
- `imports` — optional list of fully-qualified type names to import

**Result:**

```json
{
    "rewritten_source": "package com.example;\n\npublic class MyClass {\n\n    public void hello() {\n        System.out.println(\"Hi\");\n    }\n}",
    "changed": true,
    "no_op": false,
    "diagnostics": []
}
```

If the member already exists, `changed` is `false` and `no_op` is `true`.

**Errors:** `error.member_conflict`, `error.parse_invalid`, `error.type_not_found`.

---

### `shutdown`

Graceful shutdown. Returns `{"ok": true}` and the process exits 0.

**Params:** `{}`

**Result:**

```json
{
    "ok": true
}
```

## OpenRewrite Version Support

The worker uses OpenRewrite's Java 21 parser. Source files using Java 22+ syntax
(e.g., unnamed patterns, unnamed variables in all positions, scoped values) may
not parse correctly. The worker reports parse errors when it encounters
unsupported syntax. Upgrade the parser module (`rewrite-java-XX`) when
OpenRewrite ships support for newer Java versions.

## Known v1 Limitations

- **Inserted members are not auto-reflowed.** OpenRewrite's LST visitor inserts
  a syntactically-valid member (via `visitClassDeclaration`), but does not run a
  full formatting pass on the surrounding whitespace. The member text appears
  correctly positioned but indentation and blank-line layout may not match the
  file's existing style. A follow-up task: apply an OpenRewrite formatting
  recipe (e.g. `AutoFormat`) to the modified CU before `printAll()`.

- **Method identity is name + parameter count, not full signature.** The
  `insertMember` conflict/idempotency check compares method name and parameter
  *count* (plus printed-body text), not the resolved parameter *types*. Two
  same-arity overloads with different parameter types may be falsely rejected as
  `error.member_conflict`. This is fail-closed (it refuses rather than producing
  a wrong edit); a follow-up is full-signature comparison.

- **`emitType` kind is limited to class/interface/enum/record.** Annotation
  types (`@interface`) are not accepted in v1.

## Dependencies

- **OpenRewrite 8.41.0** — Java parsing, formatting, import management
- **Jackson 2.17.2** — JSON-RPC serialisation
- **Maven Shade Plugin 3.6.0** — fat jar assembly

## See Also

- `deploy/blackbox-csharp-worker/` — C# equivalent using Roslyn
- `design/refactor-tools/unified-code-synthesis-model.md` — macro model design
