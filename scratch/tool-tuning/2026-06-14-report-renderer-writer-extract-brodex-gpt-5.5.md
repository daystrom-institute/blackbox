# Report Renderer/Writer Extract Probe: Brodex GPT-5.5

Public-safe accumulation of a private-project isolate probe. All private names
and paths are intentionally omitted.

## Probe Shape

- Provider/model: Brodex GPT-5.5, high effort, priority tier
- Recipe: report renderer/writer extraction
- Target shape: a ~1,200-line Excel workbook report class mixing data assembly,
  section-specific row-writing methods (multiple fluid-type inlet writers, a
  truck-out header writer, a producer-column writer), and shared formatting
  utilities (Apache POI, spreadsheet style helpers, DecimalFormat)
- Goal: extract one inlet-writing concern into a delegate while preserving
  writer/style helper dependencies, public API shape, and workbook-generation
  behavior
- Gate: module compile

## Outcome

- Extraction applied successfully
- Cohesion clustering surfaced the right seam (cluster-3, score 0.654)
- Moved 3 inlet writer methods + 1 shared header-cell helper + 5 fields into
  a new delegate
- Writer helpers correctly threaded through delegate (cell-style object,
  header-cell helper method)
- No data-retrieval dependencies leaked into the writer delegate
- First compile failed with extraction-local errors, repaired through
  `edits.apply`, then passed
- Final compile independently verified: `BUILD SUCCESSFUL`

Real tool-use count:
- 20 exec, 12 shell_run, 7 wait, 7 bro_report, 6 todo_write, 3 tool_search,
  3 bbox_knowledge, 3 file_read, 2 bbox_inspect_entity, plus 5 others
- 68 total tool uses, 69 turns

## Friction Breakdown

### Preview exploration (8 cells)
Spent 5 exec cells + 3 wait polls on exploratory previews before finding a
clean extraction shape. The bro previewed 5 variants:
- Failed overload-ambiguous preview (shared helper method had multiple
  signatures)
- Explicit-overload preview pair
- 4-way field/method matrix exploring which helpers to move
- Constructor/wiring inspection preview (cell-style object vs DI param)
- Final clean-shape preview

Root cause: no seam-dependency preflight. The bro had to trial-and-error
overload resolution, field closure, constructor captures, and DI wireability
through full `java.extractClass` calls.

### Compile diagnostic filter chasing (7 cells)
First compile after extract failed. The `output_filter` collapsed javac errors
behind deprecation warnings. The bro iterated through 7 diagnostic approaches
before finding the real errors in an unfiltered stderr tail.

Root cause: filtered compile output hides diagnostics. On failure, the bro
needs a direct path to the raw stderr tail.

### Overload resolution failure (2 cells)
A shared helper method was overloaded (multiple signatures in the source
class). `java.extractClass` rejected with `method_overload_ambiguous`. Had to
re-run with signature-qualified names.

Root cause: cohesionClusters returns bare method names; no overload-aware
method selection primitive.

### Field initializer closure miss (3 cells)
A moved constant's initializer referenced another source constant that was not
moved. Caused a compile failure, one raw inspection cell, and one manual repair
cell.

Root cause: no transitive constant-closure analysis on moved fields.

## Retro Findings

### Recipe Changes (to fold)
1. **Overload preflight**: if cohesionClusters returns duplicate method names,
   call `code.signature` for qualified names before extractClass
2. **Field initializer closure**: for every move_fields item, inspect
   initializer for references to other source fields/constants
3. **Seam-dependency preflight**: single call before preview that reports
   overloads, field closure, remaining callers, constructor captures, and DI
   wireability — replaces exploratory preview loops
4. **Compile-gate diagnostics**: on failed compile with filtered output,
   immediately extract unfiltered stderr tail; don't iterate filter shapes
5. **Source-call verification**: after extract, query remaining source
   invocations for stale calls to moved methods
6. **Writer-specific**: treat cell-style objects and spreadsheet helpers as
   first-class writer dependencies; move the helper and style together or
   thread style via a setter/accessor

### Construct Candidates (ranked)
- **P0**: `java.extractClassPreviewPlan` — overload + closure + caller + DI
  in one dry run (eliminates preview exploration)
- **P0**: `analysis.fieldInitializerClosure` — transitive field/constant
  dependency closure for moved fields
- **P1**: `java.postExtractVerifier` — stale-call detection after extract
- **P1**: `gradle.diagnosticTail` — pull unfiltered stderr tail from failed
  shell_run
- **P2**: `analysis.writerSectionMap` — spreadsheet/POI writer inventory
- **P2**: `java.methodSelectionPreflight` — overload resolution

### Regression
Core apply path is solid — no regression from prior probes. Friction is in
preflight completeness.

## Gaps Filed
- `gap-fb25a18e`: extractClass source call rewriting + initializer dependency
  closure
- `gap-23c41e46`: bro_wait didn't return on task completion
