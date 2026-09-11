---
title: "Rust binding runtime and compiler audit"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, tools, audit, rust]
brief: "Executable coverage of every installed rust.* binding, applied edits, compiler outcomes, authority refusals, and source-integrity repairs."
---

# Rust binding runtime audit

The audit invokes the real isolate executable against temporary multi-module Rust crates. It applies proposals through `edits.merge`, `edits.createFile`, and `edits.apply`, compiles fixture tests using the installed Rust compiler, and runs those tests. It uses no model, daemon, dependency downloads, real HOME/XDG state, or operator-authority grants. Syntax parsing alone is not the success criterion.

The executable reproduction is [audit-rust-bindings-runtime.py](../../../scripts/audit-rust-bindings-runtime.py). Run `python3 scripts/audit-rust-bindings-runtime.py --isolate /path/to/isolate`. `--only` selects named cases; `--output-dir` selects a fresh evidence directory. Receipts include the executable SHA-256, installed Rust namespace inventory, exact tool outputs, applied source, compiler diagnostics, and runtime test results. Fixture and toolchain paths are normalized before export. A nonzero script exit means at least one expected contract failed.

The [complete installed-binary baseline](rust-runtime-baseline.json) has 21 cases: 8 passed and 13 failed. The failures include field-attribute loss and false trait object-safety reports. These failures include code that successfully applied and compiled while changing runtime string data, so inventory and unit coverage could not establish correctness.

The [rebuilt-binary verification](rust-runtime-verified.json) passes all 21 cases against all 15 installed bindings. Successful transforms apply through the public edits surface and compile/run; explicit authority and unsupported-mode refusals are counted as their expected contracts. The baseline and repaired binary SHA-256 identities are recorded in those receipts.

## Per-tool evidence and disposition

| Binding | Runtime evidence and compile outcome | Disposition |
|---|---|---|
| `rust.describe` | Calls all fourteen transform contracts and verifies returned identity. Unknown names refuse. | Keep the compact index plus on-demand contract. |
| `rust.extractItems` | Moves a function from `math.rs` into a child module, applies scaffold/import edits, and runs an unchanged caller. A repeat call refuses instead of reapplying. A separate mixed-visibility case keeps external-only public functions, crate visibility, parent visibility, and public fields usable through their original parent path. | Keep after preserving default visibility and re-exports, with compiler verification. |
| `rust.inlineModToFile` | Externalizes a public nested module and runs a call through the original module path. | Keep. |
| `rust.moduleWiring` | Adds a missing module declaration. Also repairs the consumer import after trait extraction. Both outcomes compile and run. | Keep the explicit small graph edit. |
| `rust.setVisibility` | Makes a private `const fn` crate-visible without dropping its qualifier, then compiles and runs a caller in another module. | Keep. |
| `rust.extractImplMethods` | Moves a method into a child file, imports its receiver, rebases `super::OFFSET`, and runs the original caller. Separate cases exercise missing and existing-empty targets with the documented short impl type name. | Keep after repairing create/edit separation and impl selection. |
| `rust.organizeImports` | Minimizes a local wildcard to the used name and runs the consumer. `mode=organize` explicitly refuses because that mode is not implemented by this binding. | Keep the implemented minimizer; do not imply rust-analyzer organization occurs. |
| `rust.moveStructFields` | Moves a field between compact structs, applies without rollback, then constructs both resulting types. A separate `#[cfg(any())]` case proves field attributes move with their field. Non-default repr refuses without a grant. | Keep after repairing declaration ranges, commas, and attribute ownership. |
| `rust.updateCallers` | Rewrites a moved `u32` field read through its delegate and executes the original accessor method. | Keep the conservative field/method distinction; field access must not become a getter call. |
| `rust.extractTrait` | Extracts a public method, applies, observes the consumer import failure, adds the reported trait import, then compiles and runs. An async-method case checks a negative `dyn_compatible` report against actual rustc E0038; a by-value `Self: Sized` method verifies the positive trait-object exemption with a compiled runtime test. | Keep with conservative syntax reporting and required compiler validation. Generic/qualified receiver impls and const methods explicitly refuse when the transform cannot preserve them. |
| `rust.liftToFree` | Moves a state-independent associated function. The old caller predictably fails; explicit `code.items` plus `edits.replace` caller repair restores compilation and runtime behavior. | Keep the documented caller-repair boundary. |
| `rust.migrateErrorType` | Migrates a private error signature and bare-variant construction mapping, preserves a matching string literal, and runs the caller. Public migration refuses without a grant. | Keep after excluding literal/comment source ranges; mapping keys are bare variant names. |
| `rust.migrateTypeUsages` | Invoked through the installed cell surface. Public API authority refusal is verified, including a separate private-only probe confirming the documented unconditional gate. No edits occur. | Retain the explicit host-authority prerequisite. The authorized audit does not manufacture its opt-out, so successful migration is not claimed. |
| `rust.rewriteModuleCallers` | Rewrites an imported and qualified call, preserves matching string/comment data, compiles, and verifies both call results and string identity. | Keep as an explicitly syntactic prefix rewrite, with required module names and bounded scope. |
| `rust.fixRound` | `build.gate` captures an actual rustc warning with anchored spans; the proposed deletion applies at `compiler_suggested` lineage and compiles/runs. Direct raw rustc diagnostics are recognized but unanchored suggestions explicitly require a fresh anchored build. | Keep the compiler repair primitive; never manufacture hashes for historical raw diagnostics. |

## Defects reproduced and repaired

1. `rewriteModuleCallers` scanned raw text and changed literal/comment occurrences as callers. `migrateErrorType` similarly changed a string containing `Old::Bad`. Both now exclude parsed literal/comment ranges before projection. Migration additionally checks the original content hash before filtering. Qualified error-mapping keys were an input-vocabulary mistake in the first experiment; the contract now explicitly says bare variant names.
2. `extractImplMethods` put a missing target in both `changes` and `creates`; the advertised recipe failed before apply with `EditSet already touches ...`. Existing empty files were incorrectly called creates too. Targets now appear in exactly one channel, and read errors no longer masquerade as missing files. The documented `Widget` selector and explicit `impl Widget` form both work.
3. `moveStructFields` removed the beginning of legal compact structs and omitted a separator before an existing final field. The choke point rolled back those invalid edits. It also left an attached `#[cfg]` behind on the next source field while inserting an unconditional destination field, a compiling semantic corruption. Removal ranges now respect declarations and attached attributes/doc comments, insertion adds the needed separator, and target-name collisions refuse.
4. `updateCallers` synthesized `self.state.count()` for a Copy field read. It now emits `self.state.count`; actual method-call expressions retain their argument list.
5. `extractTrait` retained illegal visibility qualifiers in trait implementation methods, omitted its receiver import, and missed dot-method callers in its scope report. It now removes only visibility, preserves method qualifiers, imports a simple receiver using conventional `src/` module geometry, and reports possible dot-method caller scopes conservatively. Call sites still need the trait in scope. Async methods previously yielded `dyn_compatible:true` while rustc rejected a trait object with E0038; non-dispatchable signatures now make both reported compatibility flags false unless `where Self: Sized` exempts that method from dynamic dispatch. The top-level and nested report agree, including the positive exemption case.
6. `fixRound.raw_json` accepted Cargo envelopes but silently lost direct rustc diagnostic objects. It now normalizes that explicit wire shape. Raw suggestions lack authoritative content hashes, so the tool reports the missing anchor and leaves application to a fresh `build.gate(anchor_spans=true)` result. The associated `build.gate` direct-rustc parsing repair is owned by the root audit slice.
7. `rewriteModuleCallers` advertised a 2,000-change limit but checked it only after extending a whole file's edits. It now caps before extending. Directory exclusions prune traversal; source parsing has a 1 MiB file budget and a 10,000-entry walk budget. Read, parse, traversal, and cap findings disclose incomplete coverage.

8. Compound `extractItems` replaced public item visibility with `pub(super)` and emitted private parent imports. Corrected shared edit ordering exposed the broken external caller; the baseline accidentally passed one fixture because same-offset wiring edits were overwritten. Default extraction now preserves and rebases original item/field visibility, and retains externally visible parent re-exports even without a surviving source reference. Private items retain the documented parent-access floor. Explicit visibility knobs remain overrides. The mixed-visibility executable case failed on the baseline and passes after repair.

## Limits retained deliberately

These are syntax-tier tools, not whole-program semantic refactors. Compiler and call-site checks remain required. The module caller rewrite still uses simple module segments and does not resolve aliases or split grouped imports. Trait imports use conventional file/module geometry; nonstandard `#[path]` layouts and moved-method dependencies may need explicit import repair. Dot-method scope reports are candidates because receiver types are not resolved. An unimplemented organize mode and a missing operator grant are explicit refusals, not evidence of a successful operation.

Substrate tracking: `gap-9b56878c`, dedupe `refactor_primitive/refactor-tools/rust-runtime-proposal-integrity`. The unrelated pre-existing gap file is outside this audit's ownership.
