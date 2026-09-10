---
title: "Composition, discovery, and transport contract audit"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Complete binding-interface inventory and shared invocation/transport findings at c384fb8a, with synthetic boundary probes."
---

[Audit overview](../model-facing-tools-comprehensive-audit.md). Local temporary probe paths below are provenance references; durable receipts and reproduction commands are in the overview.

# Harness composition, discovery, and transport audit

Audited checkout: `/Users/invidious/repos/transcript-search`, commit `c384fb8a1cc022f7a8bc00f94ad0462373ca1a8e`. Reference: `/Users/invidious/repos/codex`, pinned `242c5ce0`. This is a read-only implementation audit with three synthetic full-harness probes. No builds, production service changes, or real credential reads were performed by this source/probe review slice. The parent audit separately ran authenticated live model trials. The installed harness was exercised against a temporary local HTTP fixture, temporary BRO_HOME/CODEX_HOME, and synthetic authentication.

Evidence labels: **reproduced** means the installed full harness executed the counterexample; **source-confirmed** means the inspected branches establish the result without an external dependency; **risk** identifies an unexercised lifecycle or external-service consequence. Source anchors below are checkout-relative unless prefixed `Codex:`. Findings describe remaining issues after the output-limit repairs, rather than restating those repairs.

Anchor shorthand: unqualified harness files such as `mcp.rs`, `registry.rs`, `agent_loop.rs`, `code_mode.rs`, `capabilities.rs`, `bindings/...`, and `transport/...` are under `crates/bro-harness/src/`. `bro-tools/...`, `bro-code-mode/...`, and `bro-lsp/...` are under `crates/`. `Codex:` paths are relative to the reference checkout's `codex-rs/` directory.

## Main conclusion

The specialized composition layer is currently a large default product surface, not an occasional advanced option. Default optional code-mode adds a 72,240-byte exec description. Its 83 domain bindings have independent declarations, result shapes, path semantics, and capabilities; several of those contracts disagree with the executable tools. The flat and nested paths also differ in credential isolation, wrapped-tool permission enforcement, error semantics, and discovery. Keep the underlying syntax/refactor capabilities where they provide evidence or transactional guarantees, but remove their universal prompt exposure and make composition use the same invocation context as direct tools.

## Highest-priority findings

### C1. Nested shell calls lose the credential scrub context

**Reproduced, P0 isolation defect.** A flat `shell_run` under `--daemon-worker`, `BRO_HARNESS_SPAWN_SCRUB=AUDIT_CANARY`, and `AUDIT_CANARY=synthetic` prints `CANARY_ABSENT`. Identical shell code called inside `exec` prints `CANARY_PRESENT`. The probe prints presence only, never an actual credential.

Path: `agent_loop.rs:109` binds `with_spawn_scrub` around the root run future. `bro-tools/src/shell.rs:522` stores scrub keys in a Tokio task-local; `:556` silently does nothing when the task-local is absent. `bro-code-mode/src/service.rs:298` spawns the cell-control task, then `:695` spawns delegated calls without rebinding that context. `HarnessDelegate` and `HostTools` retain ToolCx but not scrub keys. Standalone daemon-worker provider credentials are process environment, so losing the scrub scope exposes them to nested shell descendants.

Recommendation: put child-process isolation policy in an explicit session invocation context carried through all delegates and process launchers; apply it at spawn regardless of async task boundaries. Audit MCP stdio and language-server children separately: `mcp.rs:436` constructs a process directly rather than using the shell scrub seam. Do not solve this by adding another prompt rule.

Coverage: existing `shell.rs:1128` tests the task-local within one future; it does not test the real V8 task transition. New synthetic receipts are in the directory named by `/tmp/harness-audit-composition-probe-root`.

### C2. A denied shell remains executable through build.gate

**Reproduced, P1 capability defect.** With `--deny-tools shell_*`, `shell_run` disappears from the wire catalog, but `exec` can call `build.gate({command:"printf AUDIT_GATE_RAN; exit 23"})`. The result reports `exit_code:23`, proving execution. The command is arbitrary shell, not a compile-command allowlist.

Path: `bindings/build_gate.rs:130` accepts `command`; `:168` constructs `bro_tools::ShellRun` and calls its body directly. The outer filter only checks `build.gate`, bypassing the shell name's deny and `call_tool_with_arg_defaults` policies. This contradicts `mcp.rs:335`'s stated use of `shell_*` denial to prevent shell access. Specialized wrappers therefore expand effective permissions beyond their advertised names.

Recommendation: remove build.gate as an execution path. Keep its diagnostic parser as a pure function over bounded shell results, or dispatch shell execution through a capability-checked shared primitive. Include it in policy-equivalence tests with direct shell calls and numeric/path defaults.

### C3. Default exec carries a specialized 72 KB manual

**Measured, P1 model-facing ergonomics defect.** Read the actual mock-Chat request fixtures from the directory named by `/tmp/harness-audit-probe-root`:

| Code-mode | Wire tools | Serialized tool-array bytes | exec description UTF-8 bytes |
|---|---:|---:|---:|
| off | 14 | 19,565 | 0 |
| optional, the default | 16 | 94,954 | 72,240 |
| only | 3 | 92,562 | 88,285 |

No MCP server was configured. Optional is about 4.9 times the off-mode tool-array size. Only-mode barely saves wire bytes because it moves the built-in catalog into the giant exec description. These measurements are tool definitions only, separate from the system prompt.

Source-only accounting, independently checked against the rendered request:

| Namespace | Methods | Description + TS declaration bytes |
|---|---:|---:|
| Java | 33 | 24,800 |
| Rust | 15 | 12,535 |
| Analysis | 8 | 8,713 |
| Code | 8 | 7,246 |
| LSP | 8 | 7,187 |
| Edits | 10 | 5,128 |
| Build | 1 | 1,828 |

The namespace sum is 67,437 bytes, plus a 4,210-byte template and section framing. All seven namespaces are installed regardless of working-set language. `agent_loop.rs:965` assembles all bindings; `code_mode.rs:248` passes every namespace description; `bro-code-mode/src/description.rs:291` renders namespaces before the optional-mode early return. Java's declarations alone are 21,774 bytes. This conflicts with the bindings AGENTS instruction that wide toolboxes should remain compact indexes with describe tools.

Recommendation: default to the small flat authoring surface while repairing composition. Make specialized namespace schemas discoverable and activate only selected methods or a chosen refactoring profile. Retain a small exec composition description. Stop loading Java and Rust workflow manuals for generic shell/file tasks. Do not merely shorten a handful of tool descriptions.

### C4. MCP result translation destroys the contract before code-mode sees it

**Source-proven, P1 data-loss defect.** `mcp.rs:580` returns only `structured_content` when present; all text content is discarded. Without structured content, `collect_text` at `:595` discards images, audio, resource links, and embedded resources. An image-only success becomes an empty successful text result. Errors retain text only and discard structured error details. A result `{content:[{type:"text",text:"Partial results; continue with cursor C"}],structuredContent:{rows:[...]}}` loses its completeness warning and continuation before the outer output cap can preserve anything.

Nested callers also receive inconsistent root shapes: `r.rows` when a server supplies structuredContent, versus a plain string otherwise. `r.content` and `r.structuredContent` never represent a normal remote MCP CallToolResult. The generic MCP examples used by Codex-trained models consequently fail even when the server followed MCP correctly.

Reference: `Codex: tools/src/tool_output.rs:182-208` retains the MCP result for tool translation and returns its serialized envelope to code-mode, deliberately stripping `_meta`. `Codex: core/src/tools/context.rs:102-156` retains the original result internally for hooks and presentation. `_meta` is client-private and should NOT be forwarded wholesale to the model. The repair is to preserve a sanitized content/structuredContent/isError envelope and internally retain private metadata, not to expose everything indiscriminately.

Coverage: `mcp.rs` has filter, placement, headers, alias, and in-process discovery tests, but no focused tests around `to_tool_result`/`collect_text` for mixed content, structured warnings, image-only results, or structured errors. In-process `McpSurface` already returns ToolResult and therefore bypasses the remote conversion defect in its tests.

### C5. Handwritten namespace declarations are neither filtered nor complete

**Source-proven, P1/P2 discovery defect.** `bro-code-mode/src/description.rs:484-539` groups the surviving tools, then emits the namespace's entire hand-authored declaration string. If only `code.items` is admitted, the model still sees declarations for seven other code methods which are absent at runtime. Authorization fails closed, but the documentation promises nonexistent capabilities.

Conversely `lsp.assist` is one of eight registered LSP tools (`bindings/lsp_facts.rs:1710`) and appears in prose, but is missing from the TS declaration in `:1968`. Its input schema exists in the installed 104-tool inventory, yet the running model has no `lsp.describe` and the flat tool_search catalog excludes cell-only tools. Schema-description success via isolate is not proof of schema visibility inside a real harness session.

ALL_TOOLS introduces another naming distinction: `bro-code-mode/src/runtime/globals.rs:135` reports `tool.global_name`, such as `lsp_assist`; that is not callable as `tools.lsp_assist`, because namespace-bound methods were skipped from tools at `:57` and installed as `lsp.assist`. Metadata has no namespace/method/schema fields. A generic discover-then-call recipe cannot execute every entry in its own inventory.

Recommendation: derive per-method declarations and invocation coordinates from the admitted catalog. Keep cross-tool types once per activated namespace. A compact runtime `describe` operation should work uniformly for flat tools, MCP tools, and namespace methods. Test filtering, inventory-to-invocation round trips, and declaration coverage over all 83 bindings.

### C6. Typed defaults and result riders break ordinary composition

**Source-proven, P1/P2 adapter defect.** `bro-tools/src/tool_defaults.rs:173-205` inserts every default as `Value::String`, irrespective of the tool schema. For example `default:code.readLines.startLine="1"` supplies a string to the binding's `usize` field, so an otherwise valid call fails deserialization. Boolean defaults have the same problem. A numeric model value also conflicts with an equivalent string pin. Validation checks parameter existence, not schema type.

`apply_rider` at `:275-310` mutates result objects, appends text to string results, and wraps primitive JSON as `{result,tool_arg_context}`. Example: `edits.begin()` is documented to return a bare id, and does at `bindings/edit_algebra.rs:173`. A global default matching the call can turn that id into an object, breaking `const es=await edits.begin(); ...`. It can also overwrite a tool's own `defaults_applied`/`pin_enforced` keys.

This is now shared by flat and nested dispatch (`registry.rs:398`; `capabilities.rs:92`), so the two paths are more consistent but both inherit the shape hazard. Rust operator grants remain host-looked-up, so model-authored acknowledgement flags cannot grant authority; however declarations claim those inputs cause schema errors while the inspected input structs do not use `deny_unknown_fields` and the dispatcher does not validate JSON schema. Such extras are silently ignored, an honesty defect rather than an authority escalation.

Recommendation: use typed JSON defaults validated against the admitted schema; keep invocation metadata out of the domain value. Separate host-only authority grants from ordinary input defaults. Add tests through HostTools for primitive results, numeric/boolean values, unknown fields, and host-authority lookup. Current registry tests mostly use string defaults and object outputs, missing these cases.

### C7. Cancellation can release the mutation gate before underlying work stops

**Source-confirmed cancellation mechanism; late patch writes reproduced by the builtin audit, P1.** `HarnessDelegate::invoke_tool`, `code_mode.rs:136`, races cancellation against `seam.call_tool` and drops the losing future. The runtime independently races and drops delegates in `bro-code-mode/src/service.rs:695`. `HostTools` holds its write guard only inside that future. Many actual tools execute work with `bro-tools/src/tool.rs:65`, which awaits `spawn_blocking`; dropping the await does not stop an already running blocking closure. A cancelled file mutation can therefore continue after its admission guard drops, allowing another mutation through.

Concrete test to add: block a synthetic mutating Tool inside call_blocking after it starts, terminate its cell, start a flat mutation, and assert the second mutation cannot enter until the first closure finishes or is cooperatively cancelled. Current shared-gate tests use cancellable async probe tools, not detached blocking work. A dropped remote MCP call has similarly uncertain remote completion, and the harness does not model that uncertainty.

Reference: `Codex: core/src/tools/code_mode/delegate.rs:176-187` explicitly awaits the submitted invocation on cancellation rather than simply discarding it; lifecycle is integrated with the central tool runtime. This does not prove every upstream side effect is interruptible. Our adapter needs an explicit completion/cancellation contract rather than a lock whose lifetime ends with observation.

## Discovery, admission, and lifecycle findings

### D1. Discovery is a permanent-growing substring search, not bounded activation

`registry.rs:439-538` matches query words by substring anywhere in name/description, ranks by number of terms matched, and returns the first eight alphabetically on ties. It searches already activated tools too, so repeating a broad query returns the same eight and cannot page to the ninth. Exact `select:` has no count cap. Activation inserts into a HashSet with no eviction or deactivation; `wire_specs:286` carries every activated schema for the rest of the session and resume. A broad search can permanently load many irrelevant tools. Large descriptions are returned unshortened in search results despite the compact-default label.

Reference: `Codex: core/src/tools/handlers/tool_search.rs:147-163` builds a BM25 search engine; `:194-249` supports a requested limit and returns loadable specs through the typed tool system. Do not assert Codex has an eviction policy from this comparison: that has not been established. Our own absence of eviction and inability to advance a broad search are source-confirmed.

Recommendation: bounded ranked discovery with explicit paging or remaining-match cursors, a limited activated schema set, and clear treatment of active matches. Preserve the recent resume-schema validation, but avoid promising infinite permanent schema residency. Tests cover initial activation, policy-constrained restoration and compact results, not catalog growth or reaching the ninth broad match.

### D2. Optional code-mode hides the schemas it allows calling

`CodeModeToolSession::new`, `code_mode.rs:229`, snapshots the full callable catalog. `build_exec_tool_description(..., false)` omits flat declarations in optional mode, and passes `deferred_tools_available=false` despite MCP tools being deferred. The JS globals still contain all admitted MCP tools. `tool_search` itself is created later in Registry, so it is not in the nested callable set. A cell can find an MCP name via ALL_TOOLS, but cannot call tool_search to retrieve/activate its input schema within that cell.

Synthetic controls are uneven by construction: `report` is appended before cm_callable is captured (`agent_loop.rs:888`) and can be called nested; `final_result` is appended after code-mode construction (`:989`) and cannot. The flat final_result is intercepted by the agent loop. This distinction can be intentional, but descriptions must tell the model where completion and discovery live rather than imply every host tool is in tools.

`report` is read-only but shares the workspace RwLock, so a long-running mutating shell call blocks subsequent report calls, including those from a sibling nested promise. Operator status traffic should not wait for a workspace mutation to finish. `notify` does not replace it: notifications are buffered until the next exec/wait response.

### D3. Old box-placement concepts create duplicate or contradictory catalogs

`mcp.rs:314` splits InBox/OutBox/Both. `agent_loop.rs:926` concatenates in-box and out-box vectors to reconstruct every tool for code-mode, including duplicate entries for Both. OutBox tools are nevertheless callable nested, so placement is not a nested-denial boundary. HostTools' HashMap collapses duplicate names; code-mode's Vec catalog does not. Codex explicitly sorts and deduplicates nested definitions (`Codex: core/src/tools/code_mode/execute_handler.rs:66-69`).

Recommendation: remove obsolete box terminology from the public configuration. Represent authorization, direct visibility, and nested availability separately only where an actual requirement differs; use one canonical deduplicated admitted tool inventory.

### D4. MCP admission omits important server metadata and failure policy

`mcp.rs:463-486` projects a server Tool into only `(name,description,input_schema)`. Read-only/destructive annotations, output schemas, title, and other metadata are not retained. Every remote MCP tool consequently receives Tool's conservative default mutation annotation and runs exclusively, even two independent declared read-only searches. `Promise.all` is advertised but all such MCP calls serialize. Codex uses read-only hints for parallel admission (`Codex: core/src/tools/handlers/mcp.rs:128-141`). Preserve the conservative fallback while retaining declared metadata and allowing policy overrides.

MCP startup loops servers sequentially (`mcp.rs:201`), has no harness-owned startup/tool timeout configuration, logs unavailability and proceeds with an absent tool catalog (`:239`). Client construction uses `().serve`, with no custom handler for tools/list-changed. The catalog is fixed for the session. A server appearing, restarting, or adding a tool leaves the session stale; an essential server's failure is indistinguishable in the model-facing tool surface from intentional absence. Compare Codex's explicit 30-second startup and 300-second tool defaults (`Codex: codex-mcp/src/rmcp_client.rs:102-103`), per-server config and reconciliation (`connection_manager.rs:332`, `connection_manager/tool_catalog.rs`). Exact lower-level rmcp timeout behavior here was not experimentally measured; the verified absence is a configurable harness policy and visible readiness/failure result.

The config accepts `type:"sse"`, but routes both Sse and Http into StreamableHttpClientTransport (`mcp.rs:448`). A legacy SSE URL is therefore not implemented by selecting this enum. Either implement that protocol or reject the misleading type. Stdio config also lacks the `exclude_tools` field available on HTTP/SSE. Invalid top-level configs and malformed per-server fields frequently warn-and-skip rather than fail clearly.

Recommendation: retain persistent per-server connections and deny filtering. Add a required/optional server policy, bounded startup, call deadlines/cancellation reporting, sanitized readiness state, catalog reconciliation, and faithful metadata. Avoid adding prompts that tell models to hunt for absent services.

### D5. Name normalization and registration have silent collision behavior

MCP qualified names use string concatenation without a collision check (`mcp.rs:207`), Registry intentionally uses last-writer-wins (`registry.rs:158`), and JS identifier normalization replaces punctuation with underscores (`bro-code-mode/src/description.rs:368`). `mcp__server__foo-bar` and `mcp__server__foo_bar` normalize to the same JS property; globals are installed with unchecked `tools.set` (`runtime/globals.rs:51-65`). Metadata can list both while one callable overwrites the other. Server/tool component names containing the qualifier separator also create ambiguous qualified names.

Recommendation: fail admission on collisions, retain canonical and JS names distinctly, and generate invocation examples from the admitted binding. The normalization itself is vendored; do not present every underlying runtime behavior as our unique invention. The missing host validation is the actionable defect.

## Transport contract matrix

| Contract | Responses | Anthropic-compatible | Chat-compatible |
|---|---|---|---|
| Function schema | parameters, strict:false | input_schema | function.parameters |
| exec representation | custom grammar, raw source | ordinary function with `{source}` | ordinary function with `{source}` |
| apply_patch | admitted with grammar | removed in assembly | removed in assembly |
| Error flag in tool result | discarded | is_error retained | discarded |
| Tool media result | impossible in transport-neutral String result | same | same |
| Malformed streamed JSON arguments | silently becomes `{}` | explicit parse error | silently becomes `{}` |

Anchors: `transport/mod.rs:143` restricts ToolResult to String plus is_error; `responses_common.rs:107` and `openai_chat.rs:352` drop is_error, whereas `anthropic.rs:724` serializes it. `responses_common.rs:342` honors grammar; `openai_chat.rs:121` and `anthropic.rs:94` render only JSON schema. `agent_loop.rs:878` removes grammar builtins before code-mode is constructed on non-Responses transports. Yet the shared exec description still says raw JavaScript, not JSON, while its actual non-Responses schema requires `{source}`. This is a transport-specific prompt contradiction, separate from the base prompt's stale tool names.

**Malformed-argument counterexample:** a model streams an incomplete JSON argument string to any zero-argument or defaulted tool. Responses `responses_common.rs:526-534` and Chat `openai_chat.rs:538-547` dispatch it as `{}`; the malformed request becomes a valid default operation rather than a repairable parsing error. Anthropic's `parse_tool_input:461` fails explicitly. Missing Chat call IDs/names are admitted when either is non-empty (`:538`), creating another malformed-call case. Schema validation is generally delegated to individual tools and is inconsistent for unknown fields.

Recommendation: one validated invocation representation, one explicit error convention in text-only providers, and per-transport honest descriptions. Keep native IDs and native custom-call output types; existing Responses tests cover those useful parts. A media-capable normalized result type is needed before adding any more image helpers. Until then all media-only MCP results must produce an explicit unsupported-result error, not empty success. Reference `Codex: core/src/tools/code_mode/response_adapter.rs:27-51` supports image and audio output items; our vendored runtime lacks that newer audio variant as well as the host media route.

The root audit separately exercised final_result schema validation and truncated Chat SSE completion. Those results should be included in the combined audit, not inferred from this matrix alone.

## Domain binding inventory and reader overlap

All 83 current domain binding names were compared against the installed schema inventory at the sibling `tool-inventory.json`; all schemas were available via isolate. This is an interface inventory, not proof of the correctness of every transform implementation.

| Domain | Surface inspected | Recommendation |
|---|---|---|
| code, 8 | files, items, fields, query, read, readLines, signature, spanUnion | Keep hash-anchored selection for edits; make it opt-in. Ordinary file browsing belongs on file_read/search. |
| edits, 10 | begin, replace, replaceText, insertBefore, insertAfter, delete, createFile, deleteFile, merge, apply | Keep transactional/hash-checked edits for complex transforms; avoid advertising a ten-step alternative for ordinary text edits. Preserve domain return shapes. |
| lsp, 8 | status, rename, references, definition, willRenameFiles, executeCommand, hover, assist | Keep semantic authority and readiness failures; fix missing assist declaration. Treat arbitrary executeCommand as a specialized control capability, not universally read-only. |
| build, 1 | gate | Remove execution wrapper or split pure parsing from shared authorized shell execution. |
| analysis, 8 | describe, cohesionClusters, references, fieldClassification, methodRegions, fieldInitializerClosure, implPartition, topLevelDeps | Keep reductions as specialized Java/Rust tools; do not put all result types in every exec prompt. |
| Java, 33 | Complete name/schema/declaration inventory; preview/consume, move, extraction, cleanup families | Keep behind explicit refactor discovery. No blanket claim that every transformer was semantically audited. |
| Rust, 15 | Complete name/schema/declaration inventory; transforms, caller repair, compiler-fix proposals | Same; preserve host-only authority and compiler/LSP lineage, remove redundant workflow prose from generic sessions. |

Reader contract differences are substantial, not cosmetic:

* `code.read` (`code_facts.rs:603-681`) requires a full-file SHA-256 span and returns exact unnumbered text with lengths and `truncated:false`. `code.readLines` (`:695-765`) uses 1-based inclusive `startLine/endLine`, returns exact text and a fresh hash-anchored span. These are useful edit-address producers, but they are not substitutes for bounded browsing. Both read the whole file into memory; neither has a maximum returned-byte parameter or hard reject limit. code.read uses UTF-8-lossy conversion while still claiming exact source bytes, so a span bisecting a multibyte character yields replacement text rather than an explicit invalid-boundary error.
* `code.items` batch (`:211-243`) accumulates every per-file inventory without an aggregate cap or continuation. That differs from code.query's aggregate capture cap (`:541`). The always-rendered recipe encourages repository-wide host fanout and a Promise.all signature request for every matching item. Printing less protects model context but does not bound Rust serialization, V8 parsing, or intermediate memory. Existing query aggregate tests do not establish an item-inventory cap.
* `code.files` returns `{files:[{file,language}],count,truncated}`; runtime items/query accept these objects directly. Their hand-written TS still types batch inputs as `string[]`, another small declaration drift. The schemas omit the required exclusive `file`/`files` relationship, leaving it to runtime validation.
* `code.*` and parts of analysis/LSP accept absolute paths outside the root as-is, and ordinary workspace tools also deliberately allow full-host paths. The path resolver is not a sandbox. This is disclosed in some schemas but contradicted by some workspace-relative-only descriptions. It must be treated as a separate capability policy, not assumed equivalent to file_read.
* LSP prose at `lsp_facts.rs:1971` calls `locations` a `Span[]`; its declaration correctly says `{span,anchored:true}[]`. A recipe following prose uses `refs.locations[0].file`, which is wrong. Code-actions are separately missing from declarations. These are precisely the small adapter-level differences that push agents back into source spelunking.
* `lsp.executeCommand` is arbitrary server-specific execution, tagged read_only at `:813`; the LSP client refuses `workspace/applyEdit` at `bro-lsp/src/lib.rs:1639`, which protects the transactional file-edit path, but does not establish arbitrary server commands as side-effect free. Keep this as a clearly scoped expert capability.

## Coverage and next verification

What is established: actual default wire sizes; complete admitted namespace name/schema/declaration inventory; executable flat-vs-nested synthetic scrub failure; executable wrapped-shell denial bypass; source paths for lossy MCP conversion, malformed JSON fallback, typed defaults, stale/overbroad declarations, fixed-session discovery, and detached cancellation.

What remains unproven: provider-model preference/quality under each reduced catalog, remote MCP timeout/reconnection behavior, actual memory-failure thresholds, cancellation of remote MCP side effects, and semantic correctness of every Java/Rust transform. Passing existing unit suites does not cover these boundaries. The current tests predominantly exercise tool bodies, synthetic in-process MCP surfaces, and a few exact output shapes.

Minimum contract suite to add after choosing the smaller surface:

1. Capture model-facing tools for each transport and off/optional/only mode with an explicit byte budget, then check every advertised invocation is executable and allowed.
2. For every tool family, compare direct and nested defaults, results, errors, cancellation, child env, and permission outcomes. Include synthetic canaries only.
3. Round-trip remote MCP text + structuredContent + error + image + resource fixtures, preserving warnings and sanitized envelopes.
4. Exercise selective namespace admission, ninth-match discovery, repeated activation, catalog changes, and resume after removal.
5. Test a cancellation barrier with blocking work, and separate operator status/control traffic from workspace execution locks.
6. Test code.read/readLines boundaries, invalid UTF-8/mid-character spans, large exact reads, and code.items aggregate limits before embedding results in V8.

The design direction should be fewer default choices and stronger shared contracts: one ordinary read/search/edit/shell workflow, optional composition, and explicit specialized refactor discovery. Preserve smart capabilities that supply semantic authority or safe transactional behavior; remove wrappers that merely rename execution, rewrite output shapes, or add ambient manuals.

## Durable probe artifacts

- [Invocation policy receipts](composition-probe-receipts.json): full-harness synthetic canary and denied-wrapper outcomes.
- [Wire receipts](wire-probe-receipts.json): default catalog byte measurements and provider terminal/schema cases.
- [Tool inventory](tool-inventory.json): all installed names and schemas.
- [Reader receipts](reader-probe-receipts.json): mid-character span and content-type extraction cases.
- [Live trials](live-trial-receipts.json): the parent audit's ten completed task runs.

The parent audit links the reusable local mock-provider scripts. Runtime byte
counts vary slightly with fixture path length; tool-array measurements are
stable for the pinned catalog. The builtin audit independently reproduced late
patch mutation after cancellation. No semantic correctness claim is made for
every domain transformer.
