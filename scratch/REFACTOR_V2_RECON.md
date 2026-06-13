# Refactor V2 Recon Notes

This scratch file accumulates anonymized shapes, processes, and capability
requirements for refactor-v2 isolate work.

Do not record private client identifiers here. No real repo names, package
paths, class names, field names, method names, file paths, business terms,
schema names, or screenshots. Keep raw evidence in disposable local scratch
state. Translate observations into generic shapes and agent capability needs.

## Goal Frame

"Make refactoring better" means making high-value structural changes cheap,
repeatable, and mechanically safe for an isolate-mode agent working in a large
Java codebase.

The goal is not a bigger catalog of one-off refactor commands. The goal is a
workflow where an agent can:

1. Recon a large class or subsystem without reading everything linearly.
2. Identify high-confidence seams with explicit risk and validation evidence.
3. Apply a bounded transform through the edit choke point.
4. Compile or otherwise gate the result.
5. Capture friction as a generic missing capability.
6. Promote repeated cell patterns into recipes only after they survive probes.

## Recon Process

Use this process when scrutinizing a private target repo:

1. Work read-only unless the operator explicitly asks for a probe mutation.
2. If mutating, use a disposable worktree or scratch checkout.
3. Keep private names out of commits, notes, gap files, docs, and prompts.
4. Record raw measurements locally only when needed for the current session.
5. Summarize findings here as anonymous shapes and capability requirements.
6. Prefer "agent must be able to..." statements over "this class needs...".

Useful recon passes:

- Size distribution: find classes/modules where line count and method count are
  extreme.
- Role distribution: UI composition, dialogs, reports, data access, scheduling,
  calculation pipelines, DTO/record aggregation, utilities.
- Framework pressure: DI annotations, route/view annotations, transaction
  boundaries, UI component binding, query DSL usage, report/export APIs.
- Churn/risk: files with many callbacks, mutable fields, constructor injection,
  final fields, generated-looking code, or high fan-in references.
- Validation gates: identify the narrow compile/test command that proves a
  local transform.

## Recon Snapshot: Large Private Java Repo

Source state: read-only reconnaissance over a private checkout. The checkout had
pre-existing probe edits, so this pass treated the working tree as evidence for
structural shape only, not as a clean baseline.

Aggregate signals:

- 2,372 tracked files visible to `rg --files`; 1,734 Java files; about 352k Java
  LOC; about 397 SQL/resource-like files.
- Java file size distribution: p50 78 LOC, p75 225, p90 527, p95 758, p99 1,522,
  max 3,983. There are 57 Java files >=1,000 LOC, 12 >=2,000 LOC, and 4 >=3,000
  LOC.
- Regex method-declaration pass found 25k+ method-like declarations across
  1,613 files; 140 files had more than 40 method-like declarations.
- UI/callback/binder-style signals: 4,687 hits across 330 files; 78 files had
  >=20 hits, 12 had >=50, and 3 had >=100.
- DI-style signals: 2,767 hits across 729 files; 72 files had >=10 hits and 30
  had >=20.
- Query-DSL/data-access signals: 4,676 hits across 473 files; 55 files had >=20
  hits.
- Report/export signals: 13,517 hits across 453 files; 117 files had >=20 hits,
  23 had >=100, and 4 had >=500.
- Co-occurrence matters: among the >=1,000 LOC files, 23 also had strong UI
  signals, 24 had strong DI signals, 12 had strong query signals, and 22 had
  strong report/export signals.
- Singleton-like classes with mutable state exist: 32 singleton-like files, 12
  with mutable field hits. This reinforces the rule that extracted delegates
  must not blindly become singleton-scoped when they carry view/request state.
- Long methods are a first-class offender: independent code-mode inventory found
  hundreds over 100 lines, more than 100 over 250 lines, and a small but
  important set over 1,000 lines. Some worst offenders are effectively "one
  method is the class."

Bro recon used four read-only isolate/code-mode workers:

- UI/dialog/form shapes.
- Data-access/admin/query-service shapes.
- Report/export/calculation-pipeline shapes.
- Meta synthesis and prioritization.

All durable notes below are genericized. Do not backfill private identifiers from
the worker transcripts.

## Worst-Offender Shapes

### UI God Surface

Large UI classes combine layout construction, injected services, mutable view
state, event listeners, data loading, validation, and child component wiring.

Agent needs:

- Rank extractable UI seams by cohesive field/method communities.
- Distinguish delegate seams from callback-heavy seams.
- Preserve framework lifecycle and route/view annotations.
- Keep mutable per-view state out of singleton delegates.
- Prefer container-constructed delegates when DI/AOP semantics matter.
- Offer a follow-up cleanup primitive for moved injection points.
- Detect framework-dispatched event handlers whose call sites are invisible to
  syntactic reference search; these usually need wrappers or callbacks, not
  caller migration.
- Trace lazy provider fields and transitive closure dependencies before moving
  dialog-orchestration methods.
- Split monolithic build methods before attempting extract-class; extracting the
  whole build method is usually not a useful seam.

### Dialog / Form Workflow Blob

Dialog-like classes often mix component construction, binder configuration,
validation, defaulting, save/cancel flows, and refresh callbacks.

Agent needs:

- Recognize binder/validator clusters as one concern.
- Preserve callback contracts back to the owning view.
- Extract helper components without breaking event ordering.
- Report whether extraction will require source-instance callbacks.
- Detect clone families across sibling dialogs/forms with near-identical method
  lists and signatures; these want a cross-file base/strategy extraction before
  local cleanup.
- Build a form-section map: component creation, binder setup, validation,
  defaulting, save/cancel, refresh callbacks.

### Report / Export Generator

Report classes combine data retrieval, row/section calculation, presentation
formatting, file/output streaming, and style/cache helpers.

Agent needs:

- Separate data assembly from formatting without changing output shape.
- Detect repeated section/table patterns.
- Extract formatting helpers with stable snapshot-style validation.
- Support "make a section object" or "extract writer/renderer" recipes.
- Detect single dominant methods inside generator classes; method-level
  extraction is the bottleneck before class-level extraction becomes useful.
- Recognize hub-and-spoke helper clusters where many sections call shared
  low-level writer/style helpers. A shared rendering context may be better than
  a source-instance delegate.
- Treat compile-only validation as insufficient for formatted outputs; require
  golden-output or structural output comparison before trusting behavior.

### Data Access / Admin Service Cluster

Admin-like classes combine query construction, mutation commands, transaction
boundaries, permission-ish checks, DTO mapping, and UI-facing convenience
methods.

Agent needs:

- Build reference counts before moving public methods.
- Classify methods by query, command, mapping, validation, and orchestration.
- Extract repository/query-object style delegates while preserving transaction
  and DI behavior.
- Keep public API wrappers when caller churn is not the goal.
- Classify callers by role: UI read-only consumer, calculation owner,
  report/export consumer, pass-through service, test.
- Preserve query-context and transaction-envelope semantics. Moving a method
  into a fresh delegate can change implicit context ownership even when the code
  compiles.
- Detect positional parameter-object pressure: very wide constructors or DTO
  constructors should trigger parameter-object/record extraction before broader
  method movement.

### Calculation Pipeline

Calculation classes contain long domain-specific formula flows, staged derived
values, unit conversions, and cross-record aggregation.

Agent needs:

- Find phase boundaries by data dependency, not just field sharing.
- Extract pure calculation stages with golden-value or compile-only fallback.
- Preserve ordering and shared mutable accumulator semantics.
- Surface when a seam is unsafe without characterization tests.
- Produce a stage dependency graph showing fetch, aggregate, derive, validate,
  and save stages, plus mutable builder/accumulator threading.
- Refuse or defer when mutable local collections/maps are read and written
  across candidate stage boundaries without clear lifetime regions.

### DTO / Record Megafile

Large DTO/record aggregation files can hold many small related shapes in one
place, often with generated or schema-adjacent structure.

Agent needs:

- Split by independent type families.
- Preserve serialization, framework reflection, and generated-code assumptions.
- Avoid broad rename/move operations without references and compile evidence.
- Detect generated-looking getter/setter slabs and avoid treating them as
  ordinary hand-authored god classes.

### Static Utility Nexus

Utility classes become central dependency magnets for UI composition, date/time,
formatting, query helpers, and conversion logic.

Agent needs:

- Build call-site clusters before moving helpers.
- Identify cohesive subsets that can become focused utility classes.
- Avoid moving helpers whose generic name hides a broad semantic contract.

### Grid / Column Builder Duplication

Large UI surfaces contain repeated fluent grid/table column construction:
fetch data, construct a grid/table, set items, then repeat long column-builder
chains. Duplication differs mostly in record type, accessor, label, key, and
formatting function.

Agent needs:

- Detect structurally identical fluent-call chains across methods.
- Extract a declarative column-spec or builder helper without losing type
  information.
- Preserve renderer/value-provider lambdas and validation/formatting callbacks.
- Offer a "spec first" refactor before trying to extract the entire view.

### Framework-Dispatched Handler Cluster

Some public methods are entry points by annotation/event bus/framework dispatch,
so `analysis.references` can report near-zero syntactic callers even though the
method is runtime-reachable.

Agent needs:

- Detect annotation/framework-dispatched methods as externally reachable.
- Default to wrappers or keep-on-source stubs unless the framework binding is
  intentionally moved.
- Include "syntactic references are incomplete" in the blast-radius report.

## Sift: Recipes vs Code Constructs

This is the useful split:

- **Recipes** are repeatable agent workflows. They can be partly prompt/recipe
  driven and may compose multiple tools. A recipe is successful when an isolate
  agent can run it with bounded cells and predictable decision points.
- **Code constructs** are substrate we need to implement: analysis facts,
  transform bindings, edit algebra support, or validation helpers. A code
  construct is successful when recipes stop requiring manual inspection or
  bespoke cell surgery for that step.

## Recipes

Promote only probe-proven loops. These recipes are the working shape we should
exercise against private code and then harden as repeated friction appears.

### R1: Large UI Section Extract

Purpose: extract a coherent UI sub-surface from a god view while preserving
framework lifecycle, event handlers, DI ownership, and mutable view state.

Flow:

1. Run cohesion clustering on the source class.
2. Pick a high-score cluster with low outbound calls and a manageable moved
   field set.
3. Run field-write/dependency analysis for candidate methods.
4. Survey public-method callers and classify framework-dispatched methods.
5. Detect provider/lazy-construction closure dependencies.
6. Extract class with moved fields, wrappers, callback externals, and DI wiring
   selected from evidence.
7. Apply through `edits.apply`.
8. Clean unused constructor params, imports, and dead source fields.
9. Compile and run the narrowest UI/static smoke gate available.

Use when: cohesion finds a real delegate seam and the cluster is not mostly
glue.

Reject/defer when: cluster score is low, cross-cluster calls swamp method count,
the candidate is a monolithic method, or provider/callback closure is deeper
than the current transform can represent.

### R2: Monolithic Method Stage Extract

Purpose: make "one method is the class" tractable before class-level extraction.

Flow:

1. Analyze a long method into contiguous statement regions.
2. Classify each region's inputs, outputs, field writes, local mutations, early
   exits, and exception flow.
3. Select one region with clean lifetime boundaries.
4. Extract it into a private helper first; escalate to collaborator only after
   repeated helpers reveal a stable object boundary.
5. Apply and compile.
6. If behavior is report/calculation sensitive, run characterization/golden
   output comparison before declaring success.

Use when: the dominant concern lives inside a single 300+ line method.

Reject/defer when: mutable local state crosses the boundary in both directions,
control flow cannot be represented cleanly, or characterization data is required
but unavailable.

### R3: Grid / Column-Spec Deduplication

Purpose: collapse repeated fluent UI grid/table column chains into a declarative
spec or shared builder before extracting larger UI sections.

Flow:

1. Detect structurally identical fluent column-builder chains.
2. Identify variable parts: record type, accessor, label, key, formatter,
   renderer, validator.
3. Introduce a column-spec or builder helper for one repeated family.
4. Replace one method family member first.
5. Compile and compare rendered/static structure where possible.

Use when: several long grid/table methods differ mostly by type/accessor/label.

Reject/defer when: lambda bodies contain divergent business rules rather than
formatting/accessor variation.

### R4: Report Renderer / Writer Extract

Purpose: separate rendering/writing from data assembly in report/export classes.

Flow:

1. Identify section clusters and the shared writer/style helper hub.
2. Extract the shared rendering context first if the hub is called by many
   sections.
3. Extract one section writer/renderer.
4. Apply and compile.
5. Capture/replay output and compare structure before trusting behavior.

Use when: a report/export class has section-like methods and shared writer
helpers.

Reject/defer when: the shared hub mutates implicit cursor/order state that cannot
be made explicit.

### R5: Query Object Extract

Purpose: isolate read/query behavior from large admin/data-access services
without breaking context ownership or public callers.

Flow:

1. Identify query-only methods and separate them from command/write methods.
2. Survey callers and classify caller roles.
3. Detect query context, transaction context, resource usage, and static
   construction sites.
4. Extract a query delegate with context ownership preserved.
5. Keep source wrappers unless caller migration is explicitly desired.
6. Apply and compile.

Use when: a subset of public methods are read-only and serve UI/report/caller
surfaces.

Reject/defer when: methods participate in a write-side unit-of-work spanning
multiple services or rely on source mutable state that is not part of the query
concern.

### R6: Calculation Pipeline Stage Extract

Purpose: separate fetch, aggregate, derive, validate, and save stages without
changing numeric behavior.

Flow:

1. Build a stage dependency graph.
2. Mark pure stages, mutable accumulator stages, write stages, and policy
   stages.
3. Extract a pure or read-only stage first.
4. Apply and compile.
5. Validate with golden values or captured output, not compile alone.

Use when: a calculation pipeline has visible phase boundaries.

Reject/defer when: ordering, mutable accumulator semantics, or domain formula
fixtures are unclear.

### R7: Parameter Object / Record Preparation

Purpose: reduce positional-constructor and wide-parameter friction before moving
methods/classes.

Flow:

1. Detect constructor/method signatures with very wide parameter lists.
2. Identify call sites and whether field names/types imply a cohesive record.
3. Introduce a parameter object or record.
4. Replace construction/call sites.
5. Compile.
6. Continue with query/UI/report extraction after call surfaces shrink.

Use when: wide constructors block safe movement or create fragile generated
calls.

Reject/defer when: constructor semantics are generated/reflection-bound or
custom equality/serialization behavior would change.

### R8: Clone-Family Consolidation

Purpose: collapse sibling classes/files that share method lists and structure
before doing local cleanup.

Flow:

1. Cluster classes by method signature skeleton and item-kind sequence.
2. Identify variable parts: entity type, accessors, labels, policy hooks.
3. Introduce a base class, strategy, or spec object.
4. Migrate one family member first.
5. Compile and probe.

Use when: three or more sibling classes are clearly copy-paste variants.

Reject/defer when: the duplicated surface hides divergent business rules or the
variable parts cannot be isolated.

## Code Constructs To Build

These are implementation targets. They should be prioritized by how many recipes
they unblock.

### C1: Field Write / State Partition Facts

Needed by: R1, R2, R5, R6.

What it returns:

- fields read by candidate methods
- fields written by candidate methods
- static/shared constants used
- mutable locals crossing a candidate region
- source fields that become dead after extraction
- "must move", "can pass", "keep on source", and "copy constant" classifications

Why: current cohesion data is useful but not enough. Extraction safety depends
on writes and mutable ownership, not just field sharing.

### C2: Caller Role Classification

Needed by: R1, R5, R7.

What it returns on top of references:

- production vs test
- UI/view-like caller
- report/export caller
- calculation/pipeline caller
- data-access/service caller
- static construction site
- framework-dispatched entry point with incomplete syntactic references

Why: wrapper vs caller migration is a role decision, not a count decision.

### C3: Method Region Analyzer

Needed by: R2, R6.

What it returns for a long method:

- contiguous statement regions
- variables defined before / inside / after each region
- outputs required after the region
- field writes
- early returns, breaks, continues, thrown exceptions
- lambdas/captures inside the region
- suggested extraction shape: helper, helper+result object, collaborator, refuse

Why: many worst offenders are not class-level extraction problems until a huge
method has been sliced.

### C4: Java Extract Method From Region

Needed by: R2, R6.

What it does:

- extracts a contiguous statement range into a private helper
- threads inputs and outputs
- synthesizes a small result object when multiple outputs are required
- refuses unsafe control flow
- preserves formatting enough for `edits.apply` and compile

Why: without this, agents manually perform the highest-risk part of monolithic
method cleanup.

### C5: Callback Externals In Isolate Binding

Needed by: R1, R4.

What it does:

- exposes callback/source-instance wiring in `java.extractClass`
- generates callback fields or functional-interface params when moved methods
  call source methods that should stay on the source
- distinguishes callback wrappers from public API wrappers

Why: source-instance callbacks currently create manual cell work and unclear
wiring choices.

### C6: Grid / Column Spec Extractor

Needed by: R3, R1.

What it does:

- detects repeated fluent column-builder chains
- extracts variable parts into a typed spec or builder helper
- preserves renderer/value-provider lambdas
- rewrites repeated methods to use the spec/helper

Why: repeated grid/table construction is a large, visible duplication class that
is too fine-grained for `extractClass`.

### C7: Query Object Extractor With Context Preservation

Needed by: R5.

What it does:

- extracts query-only methods into a delegate
- preserves query/transaction/context ownership
- threads provider/context dependencies correctly
- emits source wrappers by default
- refuses write-side or ambiguous context cases

Why: data-access classes need a transform that understands query context rather
than treating every dependency as a generic callback.

### C8: Parameter Object / Record Extractor

Needed by: R7, R5, R1.

What it does:

- introduces a parameter object or record for wide signatures
- rewrites constructors/call sites
- preserves existing serialization/reflection/generation constraints or refuses

Why: wide constructors and DTO constructors amplify every later extraction.

### C9: Report / Calculation Characterization Harness

Needed by: R4, R6.

What it does:

- captures pre-refactor output for selected report/export/calculation paths
- replays after transform
- compares serialized bytes or structured output
- separates cosmetic formatting deltas from semantic deltas

Why: compile-only gates do not protect output shape or numeric correctness.

### C10: Annotation / Framework Reachability Policy

Needed by: R1, R5, R8.

What it does:

- classifies class/method/field annotations and framework-dispatched methods
- decides copy/move/keep/refuse per annotation category
- marks methods reachable even with zero syntactic call sites

Why: framework behavior changes can compile cleanly.

### C11: Cleanup Bundle

Needed by: all transform recipes.

What it includes:

- remove unused imports
- remove dead source fields
- remove unused constructor params for supported DI styles
- organize imports after multi-file edits
- batch create files when transforms emit multiple artifacts

Why: cleanup is currently a repeated manual tail after otherwise successful
transforms.

### C12: Structural Similarity / Clone-Family Analyzer

Needed by: R8, R3.

What it returns:

- sibling class clusters by method signature skeleton
- repeated method clusters by fluent-call skeleton
- variable-part classification
- recommended consolidation shape: base class, strategy, spec, helper, refuse

Why: some refactors should start cross-file, not by polishing one class.

## Build Order

1. **C1 Field Write / State Partition Facts**
2. **C2 Caller Role Classification**
3. **C3 Method Region Analyzer**
4. **C4 Java Extract Method From Region**
5. **C5 Callback Externals In Isolate Binding**
6. **C11 Cleanup Bundle**
7. **C6 Grid / Column Spec Extractor**
8. **C7 Query Object Extractor With Context Preservation**
9. **C9 Report / Calculation Characterization Harness**
10. **C8 Parameter Object / Record Extractor**
11. **C10 Annotation / Framework Reachability Policy**
12. **C12 Structural Similarity / Clone-Family Analyzer**

Rationale: C1/C2/C3 make the agent choose better; C4/C5/C11 remove the current
manual cell grind; C6/C7 target the most common concrete shapes; C9 is required
before trusting report/calculation behavior; C8/C10/C12 broaden the surface once
the main loop is stable.

## Open Questions

- What minimum analysis lets the agent choose between caller migration,
  wrappers, external injection, and source-instance callbacks?
- When should a seam be rejected as too callback-heavy for mechanical extract?
- Which report/export refactors need characterization tests before mutation?
- How much formatting preservation belongs in transforms versus a future
  formatter validation pass?
- Which capability should be next: dependency graph, capture analysis,
  compile-gate summarization, or a second Java transform family?
- How should we represent transaction/query-context ownership generically enough
  to support multiple Java stacks?
- Should generated-looking DTO slabs be skipped by default unless the operator
  explicitly asks for generated-shape cleanup?
