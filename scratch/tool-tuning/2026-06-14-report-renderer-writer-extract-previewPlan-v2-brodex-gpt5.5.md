# Report Renderer/Writer Extract Probe: previewPlan v2 — Brodex GPT-5.5

Public-safe accumulation. All private names and paths are intentionally omitted.

## Probe Shape

- Provider/model: Brodex GPT-5.5, high effort
- Recipe: report renderer/writer extraction with previewPlan preflight
- Target shape: a ~1,200-line Excel workbook report class with fluid-type inlet
  writers, shared formatting utilities (Apache POI, spreadsheet style helpers)
- Constructs exercised: analysis.fieldInitializerClosure, java.extractClassPreviewPlan
- Gate: module compile

## Constructs Under Test

- `analysis.fieldInitializerClosure` — empty closure for this seam (independent
  string constants, no transitive deps). Confirmed working, surfaced no false
  positives.
- `java.extractClassPreviewPlan` — resolved overloads (two `createEmptyCellHeader`
  signatures), detected external callers (wrappers needed), correctly reported
  `internal_helper_deps: {}` and `ready: true`.

## Outcome

- Extraction applied successfully
- Moved 3 inlet writer methods + 2 overloaded header-cell helpers + 3 fields
  into new delegate
- Wrappers applied (has_external_callers was true)
- Overload resolution handled automatically by previewPlan
- No manual initializer-closure cells needed
- One manual repair: post-extract same-class helper wrappers for the moved
  overloaded helpers (source callers of createEmptyCellHeader still needed
  delegating stubs)
- Compile: first filtered run timed out, unfiltered rerun exposed missing
  wrapper diagnostics, post-repair compile passed, post-hygiene compile passed

## Tool Counts

Real event-log counts:

```
9  exec
9  shell_poll
7  shell_run
5  bro_report
1  each: tool_search, sandbox_grounding, bbox_note, bbox_inspect_entity,
        bbox_hybrid_search, bbox_describe_schema, bbox_bundle_evidence
───
30 total
```

## Cell Delta

| Run | exec | total | Delta |
|---|---|---|---|
| Baseline (no previewPlan) | 20 | 68 | — |
| previewPlan v1 (missing bindings) | 45 | 68 | +125% |
| **previewPlan v2 (this run)** | **9** | **30** | **-55% exec, -56% total** |

## Friction Remaining

1. **Post-extract wrapper synthesis** (gap-c2797975): when overloaded helpers
   move but same-class callers remain, source needs delegating wrapper methods.
   Currently requires manual repair via edits.replace.
2. **PreviewPlan overload normalization** (gap-6fdfc4f0): ready:true should
   guarantee resolved_methods is directly consumable by extractClass. First
   call returned ready:true with duplicate bare names; needed re-run with
   explicit suffixes.
3. **Compile-gate filtered timeout** (gap-5b424cec): first filtered compile
   timed out before terminal result. Unfiltered rerun worked but diagnostics
   were still truncated, requiring a separate log-capture command.

## Retro Findings

- previewPlan eliminated overload exploration and initializer-closure cells
- fieldInitializerClosure is working correctly (empty for simple constants)
- Internal helper dependency check correctly reported empty for this seam
- Same-class moved-helper wrapper synthesis is the next bottleneck
- Compile-gate diagnostic capture needs improvement for noisy Gradle projects

## Gaps Filed

- gap-c2797975: post-extract same-class helper wrapper synthesis
- gap-6fdfc4f0: previewPlan overload normalization contract
- gap-5b424cec: compile-gate filtered output timeout + diagnostic loss
