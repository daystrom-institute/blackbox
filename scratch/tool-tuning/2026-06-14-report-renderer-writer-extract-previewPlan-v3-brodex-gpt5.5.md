# Report Renderer/Writer Extract Probe: previewPlan v3 — Brodex GPT-5.5

Public-safe accumulation. All private names and paths intentionally omitted.

## Probe Shape

- Provider/model: Brodex GPT-5.5, high effort
- Recipe: report renderer/writer extraction with overloads_resolved blocker
  and bidirectional internal_helper_deps
- Target shape: ~1,200-line Excel workbook report class
- Constructs: overloads_resolved blocker, bidirectional internal_helper_deps
- Gate: module compile

## Outcome

- Extraction applied successfully (10 methods, 3 fields)
- Bro chose FULL cohesion cluster instead of inlet subset — recipe pushed
  toward "pick best cluster" without minimization guidance
- overloads_resolved:false despite ready:true — old daemon code, fix deployed
  after this probe
- internal_helper_deps_count:0 in final report, but bro iterated and added
  createEmptyCellHeaderLeftAligned in a prior preview attempt — friction
  hidden by final zero count
- Manual wrapper repair: 6 cells (8-13) on post-extract wrapper insertion
  for moved overloaded helpers
- Compile gate: cwd pin conflict + 180s filtered timeout → unfiltered rerun
- Final compile passed, hygiene applied

## Tool Counts

```
18 exec
 4 bro_report
 1 each: tool_search, bbox_knowledge, bbox_inspect_entity,
        bbox_hybrid_search, bbox_describe_schema, bbox_bundle_evidence, bbox_note
───
29 total
```

## Retro Findings

### Recipe Changes
1. **Cluster minimization**: seed from delegate-domain methods, expand only
   strictly required helpers. "Pick best cluster" → "Derive minimal subset
   matching the delegate name, then preview that subset first."
2. **Overload check**: if overloads_resolved:false, normalize to signature-
   suffixed names before extractClass even when ready:true.
3. **Helper deps tracking**: report helper_deps_added in final JSON, not
   just final count. A zero hides iteration friction.
4. **Wrapper synthesis**: after extract, before compile, run stale-call
   pass for moved helper names. Synthesize wrappers immediately.

### Construct Candidates
- Post-extract wrapper synthesis (gap-f4e94a82): auto-generate delegating
  wrappers for same-class callers of moved helpers
- Overload normalization in previewPlan (gap-eca8d439): already deployed
- Compile gate helper with absolute cwd + adaptive timeout
- Preview attempt accumulator: records initial blockers and resolution

### Regression
- Overload normalization fixed in latest daemon build but not exercised
  by this probe
- Inlet-subset minimization was verified in v2 run (9 exec) — confirmed
  achievable with correct recipe guidance

## Cell Delta

| Run | exec | Seam | Key |
|---|---|---|---|
| v2 (inlet subset) | 9 | 5 methods | Correct minimization |
| v3 (full cluster) | 18 | 10 methods | Recipe chose too broad |

## Gaps
- gap-eca8d439: overload normalization (fixed in ebad73ca)
- gap-f4e94a82: post-extract wrapper synthesis
