---
title: "Executable Java binding audit"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic: [bro-harness, tools, java, audit]
brief: "Runtime dispositions for every original Java binding, with applied edits and Java compilation checks."
---

# Java binding audit

This audit used the tools directly through native `isolate` cells. It covers all
33 Java bindings in the original installed catalog. Each retained transformation
was invoked on isolated Java source, its returned refs/proposals were consumed,
and proposed edits were applied through `edits.*`. Java 25 compilation and
small executable clients check the result. JDTLS was exercised against isolated
Eclipse project metadata, including explicit source roots and destination packages.
No provider/model benchmark or shared daemon was used.

The repaired catalog contains 32 Java bindings. `extractColumnSpec` was retired
because its proposal generator did not implement a coherent transformation.
`java.describe({transform:"extractColumnSpec"})` explains the retirement and
points to `code.query`, source inspection, and explicit `edits.*` proposals.
Its historical row remains in `tool-inventory.json` with a retired disposition.

## Reproduce

```sh
python3 scripts/probe-java-binding-contracts.py --isolate target/release/isolate --verify
python3 scripts/probe-java-binding-contracts.py --isolate target/release/isolate \
  --compact-layout --verify --case signature --case interface --case pullup \
  --case pushdown --case movefield --case movemethod --case inject \
  --case inject-existing --case extract --case extractblock --case addimport
```

The [executable fixture runner](../../../scripts/probe-java-binding-contracts.py)
creates a new temporary root and separate HOME/BRO_HOME/CODEX_HOME. Every case
records the authored cell, actual tool results, initial/final file hashes,
compiler result, and executable client output. `--output` selects a durable
artifact directory; existing case directories are refused. `--case` selects a
named case. `--verify` fails on unexpected refusal, failed application,
compilation error, changed observable output, omitted interface method,
incorrect relocation, unexercised installed Java tool, or a still-callable
column-spec transform. The wrappers fixture deliberately begins with the
unresolved post-extraction helper call that the tool is supposed to repair.

These are bounded source-fixture observations. Passing compilation is not proof
of binding identity, public API compatibility outside the workspace, reflection,
DI container behavior, or framework event-handler reachability. The tool-specific
limits below remain part of the recommendation.

## Reproduced defects and repairs

| Counterexample | Original observed result | Repair/disposition |
| --- | --- | --- |
| Inline `package fixture; public class Order {}` plus addImport | Applied successfully; javac rejected import after the class. | Parse package/import declaration boundaries instead of consuming whole lines. Conflicting simple-name imports now refuse. |
| `new Calc().add(2,3)` plus an added parameter | Preview found no caller; applied declaration broke the client. | Discover invocation names and argument spans from syntax captures, including receiver calls. |
| Rename parameter `x` in `return "x="+x` | Compiled, but output changed from `x=value` to `input=value`. | Rename identifier nodes; preserve strings, comments and qualified members. Ambiguous nested type/recursive argument rename refuses. |
| `@Inject private Runnable dependency;` | Apply succeeded after deleting the entire field; generated constructor referenced a missing field. | Remove annotation text without deleting its same-line declaration; preserve declaration span and constructor insertion point. Existing constructors gain the required injection annotation, and assignment insertion preserves closing-brace indentation. |
| Encapsulate a mutable static field | Apply succeeded; generated static setter referenced `this`, failing javac. | Qualify the setter assignment with its declaring class. |
| Remove unused parameter from `@Inject` ctor with `new Service(null)` caller | Apply succeeded and broke the caller. | Detect manual constructor calls and refuse before returning edits. `@Inject` alone is not exclusive construction authority. |
| Pull up `@Deprecated public int add(...)` | Apply and javac both succeeded, but target interface contained only `;`. | Strip only actual annotation syntax; preserve the selected method contract. |
| Members share a line with the enclosing class header | Several transforms copied/deleted the header as leading trivia; apply rolled back syntax errors. | Java member inventory now bounds trivia by the enclosing body and prior non-comment sibling; helper indentation contains whitespace only. |
| Extraction inserts wiring at the start of a removed member span | Valid endpoint edits were applied in the wrong order and failed parsing. | Shared edit application orders by both start and end, matching overlap validation. |
| `moveMember` method mode with `keepCopy:true` | Apply succeeded but deleted the original method, breaking the client. | Unsupported copy/visibility/prelude options refuse instead of being ignored; refs must match requested member names; recovery errors name the correct preview tool. |
| Basic repeated Vaadin column chains | Proposal used an unrelated fixed receiver, lost value providers, emitted invalid record accessors and spanned unrelated method text; apply rolled back. | Remove the callable/declaration/catalog entry. Keep a retirement explanation, not a permanently failing executable tool. |

Implementation owners: [Java bindings](../../../crates/bro-harness/src/bindings/java_transforms.rs),
[Java item inventory](../../../crates/bbox-refactor/src/java.rs),
[block extraction indentation](../../../crates/bbox-refactor/src/java/extract_code_block.rs),
and the shared edit application helper in
[bbox-refactor](../../../crates/bbox-refactor/src/lib.rs).

## Per-tool coverage and disposition

Every row refers to actual executable cases in the runner, not just schema
inspection. Preview outputs feed the associated apply call in the same cell.
The compact-layout run repeats the affected scenarios on legal single-line
classes. The separate JSON receipt records observed outcomes.

| Original binding | Executable case(s) and consumed evidence | Disposition |
| --- | --- | --- |
| java.addImport | addimport: apply, repeated no-op, conflicting import refusal; compact package/class layout | Keep, fixed declaration boundaries and conflicts. |
| java.changeSignature | signature, signature-added, signature-literals: stale ref and unreviewed-call refusal, apply, client output | Keep with syntax-only receiver limitation; fixed caller and identifier handling. |
| java.changeSignaturePreview | Same cases plus signature-overload: actual call-site/ref data and overload blocker | Keep; use binding-aware rename for ambiguity. |
| java.describe | describe: valid contract and unknown-name recovery; column: retirement explanation | Keep. |
| java.encapsulateField | encapsulate, staticfield: stale-ref recovery, apply, javac and client | Keep; fixed static setter. Caller matching remains syntax-only. |
| java.encapsulateFieldPreview | encapsulate, staticfield: field refs and accessor/reference plan consumed | Keep. |
| java.extractClass | extract: mutable counter moved, wrappers preserve client, repeat reports existing target | Keep; fixed compact member boundaries and endpoint ordering. Review dependency findings. |
| java.extractClassPreviewPlan | extract: field closure/readiness result consumed before extraction | Keep; readiness is syntax evidence, not container/runtime validation. |
| java.extractColumnSpec | column: original malformed proposal/application refusal; repaired catalog absence and describe retirement | Retire. |
| java.extractInterface | interface, annotated-interface: stale recovery, apply, explicit generated-contract assertion | Keep, fixed trivia and inline annotation loss. |
| java.extractMethodCodeBlock | extractblock: inferred capture/live-out applied and executed; extractblock-control: nonlocal return refused | Keep, fixed compact helper indentation. |
| java.fieldInjectToConstructor | inject/inject-existing: stale recovery, apply, javac and constructor annotation assertion | Keep, fixed field deletion and preserved constructor injection. Caller-arity findings still require review. |
| java.fieldInjectToConstructorPreview | inject/inject-existing: annotation flavor, constructor topology and field refs consumed | Keep. |
| java.hygiene | hygiene: disabled-all refusal, recovery, cleanup apply and javac | Keep as explicit opt-in cleanup. |
| java.inlineMethod | inline: stale recovery, apply and client; inline-state: stateful body blocked | Keep conservative supported-body gate. |
| java.inlineMethodPreview | inline, inline-state: replacement refs consumed and unsafe state blocker verified | Keep. |
| java.migrateTypeUsages | migrate: stale recovery, declared type-use rewrite, compile and client | Keep; skips construction/casts and remains simple-name based. |
| java.migrateTypeUsagesPreview | migrate: type-use refs consumed by apply | Keep. |
| java.moveClass | moveclass: missing source-root/destination refusal; moveclass-project: actual JDTLS move, external caller update, apply and javac | Keep with explicit Java project prerequisite. |
| java.moveMember | movefield/moveconstant/movemethod: stale recovery, apply and javac; movemethod-copy: unsupported option refused | Keep, simplify unsupported method-mode options. |
| java.moveMemberPreview | Three member kinds consumed; movemethod-copy rejects unsupported request | Keep with mode-specific limits. |
| java.movePackage | movepackage: missing destination refusal; movepackage-project: multi-file JDTLS move and caller compile | Keep with explicit Java project prerequisite. |
| java.normalizeWhitespace | whitespace: empty request refusal, recovery and apply | Keep; explicitly selected files only. |
| java.organizeImports | imports: missing file refusal, recovery, unused import cleanup and javac | Keep as syntax-only cleanup. |
| java.pullUpMembers | pullup: stale recovery, existing interface populated, apply and client; compact layout | Keep, fixed member trivia. |
| java.pullUpPreview | interface/pullup/annotated-interface: real candidate refs, signature and annotations consumed | Keep, fixed downstream annotation/trivia handling. |
| java.pushDownMembers | pushdown: stale recovery, concrete method transferred, apply and client | Keep; direct-header hierarchy evidence remains syntax-only. |
| java.pushDownMembersPreview | pushdown: actual member/delete spans consumed | Keep. |
| java.removeUnusedConstructorParams | unusedctor: apply; unusedctor-noinject: no-op/refusal note; unusedctor-callers: explicit manual-construction refusal | Keep, fixed exclusive-container assumption. |
| java.renameSymbol | rename: declaration and client invocation changed, compile and client output preserved | Keep only as explicitly documented simple-name operation; prefer lsp.rename for binding identity. |
| java.replaceConstructorWithFactory | factory: stale recovery, constructor/private factory/client rewrite, compile and client | Keep limited supported constructor shape. |
| java.replaceConstructorWithFactoryPreview | factory: constructor and call-site refs consumed | Keep; overloaded/anonymous/generic cases require documented review/refusal. |
| java.synthesizeHelperWrappers | wrappers: repair unresolved post-extraction call, apply, javac and client output | Keep as explicit post-extraction repair. |

## Validation receipt

Final executable verification passed **38 multiline cases and 11 compact cases**.
All 32 retained bindings were invoked; all 33 original bindings have runtime
evidence, including the retired generator. Java compilation used a clean output
directory on every check. Both JDTLS moves updated external callers; package
relocation moved two source files.

Results are recorded in [java-binding-receipts.json](java-binding-receipts.json).
The original failing observations are retained there as compact public-fixture
receipts; raw generated legacy column content is intentionally not copied.
