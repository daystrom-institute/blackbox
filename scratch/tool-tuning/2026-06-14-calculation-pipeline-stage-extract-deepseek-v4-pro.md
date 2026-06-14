# Calculation Pipeline Stage Extract Probe: DeepSeek V4 Pro

Public-safe accumulation of a private-project isolate probe. Raw private file,
class, package, symbol, and business names are intentionally omitted.

## Probe Shape

- Provider/model: DeepSeek V4 Pro.
- Recipe: calculation pipeline stage extraction.
- Target shape: a large Java calculation method containing a nested per-day
  allocation stage inside a dominant loop.
- Candidate concern: one contiguous nested calculation stage that builds a list
  consumed later in the same method.
- Gate: module Java compile with a clean target and build-cache disabled.
- Probe task: `c6a99057-3c65-43d8-8cbd-adfb8f37ca49`.
- Retro task: `bca68628-f938-4648-85e9-5837236bb175`.
- Session: `2dd5b5e5-b448-49b4-8bb9-01d50a81f4eb`.

## Outcome

Extraction succeeded.

The bro found a contiguous nested block with one live-out list, no mutated
captures, no field writes, and no non-local control flow. It resolved explicit
types for five captured inputs declared with Java `var`, then called
`java.extractMethodCodeBlock` with explicit `parameters` plus `returnType` and
`returnVar` for the live-out. The transform applied cleanly through
`edits.apply`.

The structural compile gate passed before and after `java.hygiene`. The
orchestrator independently re-ran the same compile gate; it passed in 1m56s with
the repo's existing warning volume.

Behavior validation remains absent. This probe can only claim structural safety:
the selected block was moved mechanically and still compiles. It does not
characterize the numeric behavior of the calculation stage.

## Measurements

Measured from the harness event log:

- 28 pre-retro probe tool uses.
- 19 `exec` code-mode cells.
- 4 `file_read`.
- 3 `content_search`.
- 2 `wait`.
- The retro added one additional no-tool `exec` turn, for 29 total session tool
  uses.

Diff shape in the private worktree:

- 1 Java file changed.
- 70 insertions, 72 deletions.
- Net effect: extracted helper plus call-site replacement and small hygiene
  cleanup.

Error/friction profile:

- No failed transform attempt.
- One stale-hash hygiene path required re-running hygiene after the semantic
  edit had applied.
- `analysis.methodRegions({ statementContains })` found the enclosing top-level
  loop rather than the nested stage, so the bro pivoted to marker search plus
  exact range gating.
- Captured `var` type resolution took several manual search cells.

## Retro Findings

The bro's retro identified three reusable lessons.

1. **Nested calculation stages need marker-to-range gating.**
   `methodRegions` currently filters top-level statements. In a method dominated
   by a single large loop, `statementContains` returns the enclosing loop, not
   the inner stage. The recipe should go directly from content markers to exact
   `ranges` gating when that shape appears.

2. **Java `var` type resolution is the biggest avoidable cost.**
   The region gate correctly reported captured inputs and live-outs with
   `type: "var"`, but it did not project concrete types. The bro manually found
   declaration sites, inferred simple constructor RHS types, and traced a method
   return type before calling the transform.

3. **Post-apply hygiene must use fresh state.**
   Hygiene should run after `edits.apply` against the current file state. Reusing
   pre-apply spans/hashes caused a stale path and a retry. Hygiene then applied a
   tiny whitespace cleanup, but did not fully normalize the helper body's
   indentation inherited from deep nesting.

The retro also called out the validation boundary: compile-only is structural,
not behavioral. Future arithmetic refactors inside the extracted helper need a
golden input/output characterization first.

## Recipe Changes Applied

The private calculation recipe was updated after the probe:

- warn that top-level `statementContains` can return a giant enclosing
  loop/block and tell probes to pivot to marker lines plus exact range gating;
- add an explicit `var` capture/live-out resolution step before transform;
- teach `returnType` + `returnVar` when the live-out is reported as `var`;
- prefer exact range `file_read` for the line-range-to-`oldText` bridge;
- require `java.hygiene` to run fresh after `edits.apply`;
- record deep-nesting indentation residue as formatting quality instead of
  hand-editing unless parse/compile fails.

Private recipe commit: `a369b7e34`.

## Gap Notes Filed

- `gap-40d8c31a`: resolve Java `var` declaration types for extract-method
  captures/live-outs.
- `gap-1eacd86a`: analyze nested Java method regions, not only top-level
  statements.
- `gap-e03f8661`: normalize moved Java helper body indentation after
  deep-nested extraction.

## Next Action

The calculation recipe is worth keeping, but the immediate next move is recipe
refinement plus a small type-resolution construct, not another rerun of the same
target. The probe already succeeded and exposed the ergonomic bottleneck.

## Reprobe After Nested Regions And Resolved Types

Second run after adding `includeNestedStatementRegions` and syntax-only
`resolved_type` hints:

- Probe task: `aa0647dd-9a3a-4eb6-83fe-62b688e54d0a`.
- Retro task: `9e6ecce9-6dd1-422b-8365-596ab86455f9`.
- Session: `fc516b86-b68e-41cd-9e23-f14b8486b4b3`.
- Private recipe tip: `c73cc28cb` at dispatch; updated to `3c969f7b3` after
  retro feedback.

### Reprobe Outcome

Extraction succeeded again on the same target shape: a nested Java calculation
stage inside a large loop. The new tooling changed the path:

1. Top-level `statementContains` returned the enclosing loop, as expected.
2. `includeNestedStatementRegions:true` surfaced inner statement markers, which
   were used to gate the exact range.
3. `analysis.methodRegions` reported a clean contiguous block with one live-out,
   no mutated captures, and no non-local control flow.
4. `resolved_type` handled most `var` boundary facts automatically.
5. Two `var` captures still lacked `resolved_type` because their RHS forms were
   method-call/static-factory-like; the bro manually traced those types and
   passed explicit `parameters`.
6. `java.extractMethodCodeBlock` applied cleanly, then compile, hygiene, and
   recompile all passed.

The orchestrator independently reran the clean no-build-cache compile gate:
`BUILD SUCCESSFUL in 2m 6s`.

Behavior validation is still structural-only. There is no golden output for the
numeric calculation stage.

### Reprobe Measurements

Pre-retro probe tool-use profile:

- 39 total tool uses.
- 15 `content_search`.
- 11 `exec`.
- 7 `file_read`.
- 4 `shell_poll`.
- 2 `shell_run`.

Diff shape:

- 1 Java file changed.
- 70 insertions, 72 deletions.

One real tool error occurred: the bro tried `content_search(...)` inside a
code-mode exec cell. It recovered by using direct tool calls. This is recipe
wording friction, not a failed refactor primitive.

### Reprobe Retro Findings

- `includeNestedStatementRegions:true` materially improved discovery. It did
  not eliminate the exact range read, but it removed manual reasoning about
  nested loop structure.
- `resolved_type` materially improved `var` handling, but only for locally
  derivable syntax forms. Method-call/static-factory RHS forms still require
  manual tracing.
- `analysis.methodRegions` still does not return exact range text, so
  `oldText` requires a separate bounded read after gate acceptance.
- Deep-nesting helper-body indentation residue reproduced after hygiene. This
  keeps the formatting gap open.
- Clean compile dominates wall-clock time; recompiles after hygiene should avoid
  `clean` unless the first gate revealed generated-source/cache ambiguity.

### Reprobe Recipe Changes Applied

The private calculation prompt was updated after retro:

- tell probes to call `content_search` directly rather than as a bare exec-cell
  function;
- name method-call/static-factory RHS tracing as the fallback when `resolved_type`
  is absent;
- state that a separate exact range read is required for `oldText`;
- compile after hygiene without `clean` unless generated-source/cache ambiguity
  was observed.

### Gap Notes

- New `gap-25ea6684`: resolve Java `var` types through method-call and
  static-factory RHS patterns.
- Updated `gap-5537d927`: direct line-range / exact range read ergonomics; this
  reprobe still needed a separate read to produce `oldText`.
- Updated `gap-e03f8661`: helper-body indentation residue reproduced.

### Next Action

Stop rerunning this exact calculation target until one of the remaining
construct gaps changes. The highest-value next code construct is method-call /
static-factory `var` type resolution; the most visible polish construct is
helper-body indentation normalization.
