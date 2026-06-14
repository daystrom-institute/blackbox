# Monolithic Method Stage Extract Probe: DeepSeek V4 Pro

Public-safe accumulation of a private-project isolate probe. Raw private file,
class, package, and business names are intentionally omitted.

## Probe Shape

- Provider/model: DeepSeek V4 Pro.
- Recipe: monolithic method stage extraction.
- Target shape: an ~826-line Java/Vaadin dialog-builder method inside a large
  UI class.
- Candidate concern: a controls sub-surface inside the long method.
- Gate: module compile with JDK 25.
- Probe task: `b37f3d64-a3ed-4e3a-928e-d984b4bfcafa`.
- Retro task: `04d7173d-cc44-47f7-a987-c34780bb1020`.
- Session: `d3aec229-3989-40d1-ab40-1f1e76dc6159`.

## Outcome

No extraction was applied.

The bro found an existing lower-level Java code-block extraction primitive:
`extract_java_code_block_to_method`. That is useful: the monolithic-method
recipe is not completely unsupported, but it currently lives behind the generic
refactor-plan surface rather than the `java.*` code-mode namespace.

The selected logical concern was not a single clean block. It was spread across
four interleaved blocks inside the long method, separated by unrelated setup.
The bro then tried the most self-contained contiguous sub-region. The planner
refused with `error.multi_return_needs_record`: six locals declared in the
candidate block were read after the block. The planner recommended introducing
a record/result object, which would have required manual surgery and was
therefore correctly rejected by the probe protocol.

Baseline compile passed independently after the probe:

```bash
JAVA_HOME=/opt/homebrew/opt/openjdk@25 PATH=/opt/homebrew/opt/openjdk@25/bin:$PATH ./gradlew :webapp:compileJava --console=plain
```

Result: `BUILD SUCCESSFUL`.

## Measurements

Actual pre-retro tool-use count from the event log:

- 35 total tool uses
- 14 `file_read`
- 10 `exec`
- 2 `todo_write`
- 2 `shell_run`
- 2 `shell_poll`
- 2 `content_search`
- 1 `smart_read`
- 1 `mcp__blackbox__bbox_knowledge`
- 1 `list_dir`

Error-bearing tool results:

- wrong-root file read before switching to the worktree-rooted path
- namespace confusion: attempted `mcp__blackbox__java_describe` instead of the
  bare `java.describe` code-mode namespace
- attempted to search harness dump paths from inside the restricted worktree
- one swapped start/end line read
- first code-block extraction attempt omitted the required helper name
- second code-block extraction attempt refused with
  `error.multi_return_needs_record`

## Retro Findings

The bro's retro reported 20 code-mode cells, 4 errors, and 2 retries. It called
out three blocking construct gaps:

1. No auto-generated record/result-object companion for multi-live-out method
   extraction.
2. No non-contiguous-region extract-helper transform for interleaved UI-builder
   concerns.
3. No automated method-body region analysis; concern boundaries, live-outs, and
   captures were manually inferred from raw slices.

It also noted avoidable workflow friction:

- plan-kind discovery was too broad and produced large dumps;
- code-mode namespace bindings are easy to confuse with MCP tool names;
- simple identifier location was faster and cleaner through content search than
  a broad tree query;
- long method reading should start with outline/summary, then bounded ranges.

## Recipe Changes Applied

The private monolithic-method recipe and prompt were updated after the probe:

- add a **contiguity gate** before transform attempts;
- add a **live-out gate** before transform attempts;
- document the lower-level `bbox_refactor_plan` path for
  `extract_java_code_block_to_method`;
- tell probes to stop instead of hand-building a result object;
- remind code-mode users that `java.*`, `analysis.*`, `code.*`, `edits.*`, and
  `lsp.*` are bare namespace globals, while `bbox_*` tools remain under
  `tools.mcp__blackbox__*`;
- prefer outline plus bounded range reads over full-method dumps.

## Code Construct Candidates

- `java_method_body_region_analysis`: partition a long method into
  concern-labelled contiguous regions with read-before, live-out, field
  read/write, lambda/listener capture, and early-exit facts.
- `java.extractMethodCodeBlock`: code-mode namespace binding around the
  existing plan kind, returning edits-compatible payloads and focused contract
  text.
- `auto_generate_java_record_from_block_outputs`: generate and thread a small
  result record for multi-live-out block extraction.
- `extract_java_discontiguous_regions_to_method`: guarded extraction from
  multiple ordered ranges when a logical concern is interleaved with unrelated
  setup.

Generic substrate gap notes were filed for these candidates:

- `gap-df4938f7`: Java record bundles for multi-live-out extract-method regions.
- `gap-d8699272`: Java method-body region analysis.
- `gap-49e2ce33`: discontiguous Java region extraction.
- `gap-762ec9f7`: code-mode Java extract-method binding.

## Next Action

Do not rerun this exact probe yet. The next useful move is code/recipe work:
either implement method-body region analysis first, or add the `java.*`
extract-method binding plus a live-out preview/report before trying another
monolithic method.

## Rerun After Method-Region Gates

Second probe after adding the direct code-mode bindings:

- Probe task: `93e8d074-b02e-4d07-9840-50fec6083ebe`.
- Retro task: `f720ebb1-9f43-4ae0-92f4-b7ef783e635b`.
- Session: `9ae9ebee-4e91-4768-83c5-c639f4ed7bd6`.
- Private recipe branch tip: `5684f0b46`.
- Target shape: same large Java/Vaadin dialog-builder method and same controls
  sub-surface, generalized here to avoid private project identifiers.

### Rerun Outcome

The new direct path worked:

1. The bro verified JDK 25 from daemon-propagated `JAVA_HOME`.
2. It found `analysis.methodRegions`, `java.extractMethodCodeBlock`, and
   `java.hygiene`.
3. It used `analysis.methodRegions` to reject broad/tight stage ranges, then
   selected a smaller listener-registration block with zero live-outs and no
   non-local control flow.
4. It called `java.extractMethodCodeBlock`, applied the returned edit set with
   `edits.apply`, compiled, ran `java.hygiene`, applied hygiene, and compiled
   again.
5. The orchestrator re-ran the compile gate independently with JDK 25:
   `BUILD SUCCESSFUL in 1s`.

Compile success is not the whole story. Diff review found the generated helper
was compile-valid but badly formatted:

- call-site indentation was lost;
- the new helper began on the same line as the previous method's closing brace;
- helper-body indentation was collapsed;
- `java.hygiene` sorted imports and removed some blank lines, but did not repair
  helper insertion spacing or moved-body indentation.

This turns post-transform formatting into a real construct gap, not a cosmetic
note. Future probes should not call a compile-valid helper extraction fully
successful until they inspect the call site/helper formatting or a stronger
Java formatter/hygiene primitive exists.

### Rerun Measurements

Measured from the harness event log before retro:

- 30 total probe tool uses
- 11 `exec` code-mode cells
- 13 `file_read`
- 3 `shell_run`
- 1 `smart_read`
- 1 `glob`
- 1 `content_search`
- 0 error-bearing tool results

The retro was one additional `exec` cell in the same provider session.

### Rerun Retro Findings

The bro and orchestrator found these recurring substrate gaps:

1. **Lambda-local return classification.** The region analyzer counted returns
   inside validator/listener lambdas as non-local control flow. For an
   extraction where the whole lambda expression moves inside the helper, those
   returns are local to the lambda and should not block the broader stage.
2. **UI-tree live-out over-reporting.** A component declared inside the candidate
   block but consumed by component-tree wiring inside that same block was counted
   as a true live-out. The method return value was also counted like an ordinary
   live-out. This made the broader region look like a multi-output extraction
   even though part of the state was already tree-consumed.
3. **Method-region output was too broad.** A 178-statement region report pushed
   the bro into harness dumps and follow-up bounded reads. `methodRegions` needs
   a compact/filter/search mode for long methods.
4. **`code.read` needs truncation metadata.** The bro burned a cell verifying
   whether returned text was truly truncated or only display-truncated.
5. **Line-range gates should warn or expand.** The bro had to re-run a gate
   because the first line range ended just before a statement that logically
   belonged to the listener block.
6. **Java extraction formatting/hygiene is insufficient.** The transform and
   current hygiene compose to a compile-valid result, but not a professionally
   formatted result.

### Gap Notes Filed

- `gap-8516bf36`: Java method-region NLOCF should treat lambda-local returns as
  local.
- `gap-d7e01105`: Java method-region live-out facts need UI tree-consumption
  classification.
- `gap-c21ea5e8`: `methodRegions` needs filtered compact output for very long
  methods.
- `gap-df0d3979`: `code.read` should report whether returned text was
  truncated.
- `gap-4f021516`: Java extract-method/hygiene should preserve helper
  formatting, indentation, and method spacing.

### Updated Next Action

Do not rerun this exact probe again until the extraction formatting/hygiene gap
is addressed. The next code tranche should either:

- fix `java.extractMethodCodeBlock` insertion/body indentation and/or upgrade
  `java.hygiene` to repair helper spacing and indentation; or
- refine `analysis.methodRegions` for lambda-local returns and UI-tree
  live-outs so the broader stage can be gated accurately.

## Rerun After Lambda/Formatting Fixes

Third probe after public commit `e2e7c41d` and the matching private recipe
update:

- Probe task: `9fb860e8-ab62-419e-a5b8-fa719150c1d8`.
- Retro task: `27b01cf5-3e22-4999-97ac-fc2840361c5e`.
- Session: `72b138a9-d2c4-4d1f-bf36-41657881bac6`.
- Target shape: same large Java/Vaadin dialog-builder method and same controls
  sub-surface, generalized here to avoid private project identifiers.

### Third Rerun Outcome

The direct pipeline worked end-to-end:

1. The bro verified daemon-propagated JDK 25 before the compile gate.
2. It used `analysis.methodRegions` to gate a contiguous listener-registration
   region.
3. The selected region had five top-level statements, 12 captured locals, zero
   live-outs, zero mutated captures, and zero non-local control flow.
4. Returns inside fully selected lambdas/listeners were not reported as
   method-level non-local control flow. The lambda-local return fix removed the
   previous false stop.
5. `java.extractMethodCodeBlock` returned two edits with no FIXMEs; the bro
   applied them through `edits.apply`.
6. Compile passed, `java.hygiene` made one touched-file cleanup, and compile
   passed again.

The orchestrator independently reran the compile gate with JDK 25:
`BUILD SUCCESSFUL in 1s`.

Diff review confirmed the formatting fix worked:

- call-site indentation was preserved;
- the inserted helper was separated from the enclosing method with normal method
  spacing;
- moved-body indentation was preserved;
- hygiene only performed import/blank-line cleanup.

No new substrate gap was filed from this rerun.

### Third Rerun Measurements

Measured from the harness event log:

- 26 total probe tool uses
- 11 `exec` code-mode cells
- 10 `file_read`
- 2 `shell_run`
- 1 `content_search`
- 1 `glob`
- 1 `sandbox_grounding`

The retro added no tool calls. There was one recoverable code-cell scope error:
the bro tried to reuse a JavaScript local from a prior exec cell, then recovered
by re-running the gate/read sequence in one cell.

### Third Rerun Retro Findings

The retro reported no missing primitive. The remaining waste was recipe
sequencing:

- for methods over roughly 200 lines, search for a marker first and gate a
  bounded range instead of dumping the full method inventory;
- keep gate -> exact read -> transform -> apply in one exec cell where practical,
  because exec cells do not share JavaScript local scope;
- trust compile `exit_code: 0` on successful gates, and read shell dumps only on
  failure or targeted warning/debug inspection.

The private recipe and prompt were updated with those steers after the rerun.

### Current Next Action

This exact monolithic-method recipe no longer needs another immediate rerun. The
remaining construct candidates are the broader ones still not exercised by this
successful listener-only extraction:

- UI/component-tree consumed-local classification for Java live-out gates;
- compact/filter/search mode for `analysis.methodRegions` on very long methods;
- extract-method preview/result-record generation for multi-live-out block
  extraction;
- discontiguous-region extraction for interleaved UI-builder concerns.

## Probe After Compact Method-Region Gates

Fourth probe after adding compact/filter output and UI-tree live-out
classification to `analysis.methodRegions`:

- Probe task: `d4655d60-d8c3-41b6-b14f-aea4683c089b`.
- Retro task: `f1631d67-dcfb-4f5c-b384-ab84801f008d`.
- Session: `27ac61e3-60b4-4872-98be-1f160d1eb6ce`.
- Target shape: same large Java/Vaadin builder method and split-controls
  candidate, generalized here to avoid private project identifiers.

### Fourth Probe Outcome

No mutation was applied. This was the correct stop.

The probe used the newly added compact path directly:

1. `statementContains` plus `statementLimit` reduced a 178-statement method to
   15 matching statement regions and reported `matched_count`, `omitted_count`,
   `returned_count`, and `total_count`.
2. `includeStatementRegions:false` returned gate-only output for exact candidate
   ranges without materializing full statement inventories.
3. The broader split-controls concern was confirmed as non-contiguous across
   three clusters.
4. The most cohesive contiguous controls-assembly block had six live-out locals,
   so current `java.extractMethodCodeBlock` correctly could not apply.
5. `after_use_kinds` and `component_tree_consumptions` made the live-out
   decision much clearer: some UI locals were wired into the component tree in
   the candidate block, but still had post-region receiver/argument uses and
   therefore remained real outputs.
6. The adjacent listener block was extractable by itself, but the probe avoided
   repeating the previously solved smaller extraction because it would not
   address the broader concern under test.

### Fourth Probe Measurements

Measured from the harness event log before retro:

- 18 total probe tool uses
- 5 `exec` code-mode cells
- 9 `file_read`
- 2 `content_search`
- 1 `glob`
- 1 `mcp__blackbox__bbox_note`
- 0 error-bearing tool results

Retro added 4 tool calls: 2 `file_read`, 1 `exec`, and 1 done note.

### Fourth Probe Retro Findings

The new `methodRegions` controls were a decisive improvement:

- `statementContains` / `statementLimit` avoided the old broad 178-statement
  dump path.
- `includeStatementRegions:false` made exact range gates compact.
- `after_use_kinds` and `component_tree_consumptions` were sufficient for the
  live-out decision; no manual live-out counting was needed.
- Lambda-local returns stayed local; no non-local-control-flow regression
  appeared.

Remaining waste was recipe sequencing, not a new code gap: the bro still used
several bounded raw reads to map the method after the compact statement search.
The retro recommendation is to gate directly from the compact
`statementContains` line ranges, and only read source once a transform needs
exact `oldText` or the gate is ambiguous.

### Updated Next Action

The next code construct should be result-record/result-bundle generation for
multi-live-out Java extract-method regions. This probe reaffirmed
discontiguous-region extraction as a real second-order construct, but the
single contiguous block with multiple outputs is the closer, higher-leverage
next step.

## Rerun After Result-Record Generation

Fifth probe after adding `resultRecord:true` support to
`java.extractMethodCodeBlock` and updating the private recipe to exercise a
multi-live-out contiguous block:

- Probe task: `ff3f0200-1d3c-4c93-93a2-66f7c3c01cd2`.
- Retro task: `38e34aec-2936-4038-b6cb-7e261de0aa92`.
- Session: `d8fa5fa9-61b1-41bd-82f3-4b7b3501e8bf`.
- Private recipe branch tip: `3e6d4a80a`.
- Target shape: same large Java/Vaadin builder method, generalized here to
  avoid private project identifiers.

### Fifth Probe Outcome

The result-record path worked for the intended shape: a contiguous block with
six live-out locals was extracted into a helper returning a private nested
record, with call-site unpacking for the returned values.

The generated result-record mechanics were sound, but one captured input local
had been declared with Java `var`. The transform propagated that inferred token
into the helper parameter list, which is not valid Java. The bro repaired the
signature manually in the disposable worktree before the final compile. That is
not acceptable as the steady-state workflow, but it is useful evidence for the
next construct.

Independent validation used a cache-busting compile gate after the probe:

```bash
./gradlew :webapp:clean :webapp:compileJava --no-build-cache
```

That gate completed successfully after the disposable repair.

### Fifth Probe Measurements

Measured from the harness event log before retro:

- 40 total tool uses
- 12 `file_read`
- 11 `exec`
- 5 `shell_run`
- 4 `todo_write`
- 4 `content_search`
- 2 `glob`
- 1 `smart_read`
- 1 attempted direct knowledge lookup from code mode

Recoverable errors observed:

- code-mode local scope did not persist across exec cells;
- direct blackbox knowledge lookup was not available inside code mode;
- one edit attempt used a stale string after hygiene/reformatting changed the
  source.

### Fifth Probe Retro Findings

The documented retro was run by resuming the original session with
`bro_resume`, not by `bro_retro`.

The retro identified one main code construct and several recipe/process
adjustments:

- Implement a Java capture-type resolver for extract-method parameters. If a
  captured input local is reported as `var`, the transform should resolve a
  concrete parameter type or fail closed before producing edits.
- Keep gate -> bounded read -> transform -> apply in one code cell when
  possible. Exec-cell JavaScript locals are intentionally scoped to one cell;
  cross-cell state needs explicit persistence.
- After any orchestrator-side reset, revert, or worktree cleanup, verify file
  state before rerunning analysis. The earlier probe confusion was caused by
  external worktree mutation while the bro still owned the worktree.
- Select candidate ranges that actually exercise the desired construct. A
  zero-live-out listener-only extraction is useful for basic extraction, but it
  does not probe result-record generation.
- Treat cached Gradle success as insufficient after semantic source edits. Use
  `--no-build-cache` plus a clean/recompile step or an equivalent proof that the
  edited source was compiled.

### Fifth Probe Gaps

Filed public, sanitized gap notes:

- `gap-ce7174ea`: resolve Java `var` captures before generating helper
  parameters.
- `gap-b79620d1`: probe compile gates should avoid build-cache false positives
  after edits.

### Updated Next Action

Implement the Java capture-type construct next. The narrowest acceptable first
step is fail-closed behavior for `var` captures in generated helper parameters;
the better construct is a resolver that can infer concrete local types from the
enclosing method source and feed explicit parameter metadata to
`java.extractMethodCodeBlock`.
