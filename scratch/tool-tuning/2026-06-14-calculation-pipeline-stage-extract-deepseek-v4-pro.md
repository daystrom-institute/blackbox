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
