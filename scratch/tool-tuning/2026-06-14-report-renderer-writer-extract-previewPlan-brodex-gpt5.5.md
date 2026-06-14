# Report Renderer/Writer Extract PreviewPlan Probe: Brodex GPT-5.5

Public-safe accumulation of a private-project isolate probe. All private names
and paths are intentionally omitted.

## Probe Shape

- Provider/model: Brodex GPT-5.5, high effort
- Recipe: report renderer/writer extraction with `java.extractClassPreviewPlan`
- Target shape: a ~1,200-line Excel workbook report class mixing data assembly,
  section-specific row-writing methods, shared writer/style helpers, constants,
  and workbook-generation orchestration
- Goal: measure whether the new preview-plan preflight reduces the baseline
  extraction cell cost by replacing exploratory preview loops, overload
  resolution, and field-initializer closure inspection
- Gate: module compile

## Outcome

- Extraction applied successfully
- Hygiene applied successfully
- Final compile passed
- `java.extractClassPreviewPlan` was used before `java.extractClass`
- `previewPlan.ready` was false because the selected writer method still had a
  source-side caller, so the bro used exactly one allowed `previewOnly` call
- `previewPlan.resolved_methods` and `previewPlan.augmented_move_fields` were
  used for the real extraction
- Manual overload-resolution cells were eliminated
- Manual field-initializer closure cells were eliminated
- Total cell count did not decrease; costs shifted into binding
  discoverability, compile diagnostics, and private helper-dependency repair

Real tool-use count before retro:
- 45 exec, 23 wait
- 68 total tool uses by the requested event-log grep

Baseline comparison:
- Baseline: 20 exec, 12 shell_run, 7 wait, 68 total
- PreviewPlan run: 45 exec, 0 shell_run surfaced by the grep shape, 23 wait,
  68 total
- Delta: +25 exec, -12 shell_run, +16 wait, 0 total

## Friction Breakdown

### Preview-plan success

`java.extractClassPreviewPlan` did the intended overload and initializer-closure
preflight work. The run spent no manual cells resolving overload-qualified
method names and no manual cells inspecting transitive field initializer
dependencies.

This validates the two P0 constructs for the specific baseline friction they
were designed to remove:
- overload resolution was supplied by the preview plan
- field closure was supplied by the preview plan's augmented move-field set

### Binding discoverability (1 cell)

The recipe named the transform as `java.extractClassPreviewPlan`, but the probe
harness exposed the callable through a generated nested-tool alias. The bro
retried the same mandatory preflight through that binding.

Root cause: describe output and callable namespace did not line up in this
harness. The next recipe revision should either name the fallback alias or
instruct the bro to inspect available tool names when a described transform is
not present on the namespace global.

### Private helper-dependency repair (7 cells)

The selected writer section depended on private writer/header helpers that were
not moved and were not flagged by previewPlan. The first raw compile surfaced
missing-helper errors, and the bro repaired the extraction by moving the small
writer helper and threading a runtime header style into the delegate method.

Root cause: previewPlan closed over overloads, moved fields, external callers,
and initializer dependencies, but not private method dependencies used by moved
methods. This shifted the old preview exploration cost into post-apply compile
repair.

New gap:
- `gap-5fed070f`: extractClass planning needs private helper dependency closure
  for moved methods, especially writer/header/style helpers.

### Compile diagnostics (13 cells)

Compile validation was the largest remaining cost. The filtered compile first
timed out during normal generated-source work. The required raw rerun then
failed, but the retained tail mostly contained warning summaries rather than the
first Java error block. The bro had to inspect the generated delegate and source
rewrites directly to identify the missing helper dependency.

Root cause: the compile wrapper preserved final status and warning summaries
better than the first actionable Java errors. For this codebase shape, generated
source work and warning volume can make a healthy or repairable compile look
like a long-running/noisy failure.

### Section selection and verification (4 cells)

The bro still manually identified a bounded writer section and verified that
data-retrieval dependencies did not leak into the extracted delegate. PreviewPlan
does not yet provide a report-section map or writer-helper dependency inventory.

Root cause: no section-map fact exists for report/export classes. Cohesion
clustering identifies candidate clusters, but the bro still had to read and
classify section behavior manually.

### Post-extract hygiene and stale-call checks

The bro verified that the source retained only a qualified delegate call, then
ran Java hygiene over the touched files and compiled again. This was useful
closeout work, but it added cells that are orthogonal to the previewPlan
preflight measurement.

## Retro Findings

### Recipe Changes (to fold)

1. **Preview-plan binding note**: if `java.describe` documents
   `extractClassPreviewPlan` but `java.extractClassPreviewPlan` is unavailable,
   inspect the generated tool aliases and call the preview plan through the
   available binding.
2. **Helper-dependency preflight**: before apply, require the plan to identify
   private helper methods called by moved methods. If unavailable, inspect the
   moved method's private calls or choose a cluster that already contains the
   helper methods.
3. **Success metric split**: distinguish preview exploration reduction from
   total cell reduction. PreviewPlan can eliminate overload/initializer cells
   while total cells still rise from validation and repair.
4. **Compile diagnostics**: use a longer timeout for generated-source-heavy
   builds and preserve the first Java error block, not only the final warning
   summary.
5. **Writer-specific**: treat header-cell helpers, row/column writer helpers,
   and runtime style objects as part of the writer dependency closure.

### Construct Candidates (ranked)

- **P0**: private helper dependency closure in `java.extractClassPreviewPlan`
  and/or `java.extractClass`
- **P0**: compile diagnostic helper that preserves first Java errors plus final
  status while filtering generated-code warning noise
- **P1**: report/export section-map facts showing section methods, helper
  dependencies, and data-access boundaries
- **P1**: transform binding resolver that maps documented namespace transforms
  to the actual callable binding exposed in the harness
- **P2**: higher-level writer-context primitive for header/style/helper bundles

### Regression

No regression in the apply path. The extraction, manual repair, hygiene, and
final compile all completed. The regression relative to the hoped-for
measurement was in total cell count: previewPlan removed the target exploration
cells, but missing helper closure and validation friction consumed more cells
than it saved.

## Gaps Filed

- `gap-5fed070f`: extractClass planning needs private helper dependency closure
  for moved methods.
