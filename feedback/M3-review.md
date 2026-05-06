# M3 + G2 fixes review

Commits `4b53bc2..235b082` (4 G2 fixes + M3 auto-digest arc).

## Issues (fix-forward)

1. **No worktree isolation for the digest-extractor LLM dispatch.**
   Per H3 fix #2, the eval LLM runs in a worktree to contain
   modifications. The digest-extractor brofile has no such
   protection. The lens says "Output strict JSON only" but LLMs can
   disobey; if the LLM decides to call Edit/Write/Bash via the bbox
   tool surface (which is exposed since the brofile carries no
   `disallow_tools`), it could pollute the corpus. Either:
   - Add `disallow_tools: ["Edit", "Write", "Bash"]` to the
     digest-extractor brofile (read-only by construction).
   - Run the actor turn in a worktree (heavier).
   The first is one-line + much safer.

2. **`source_query` isn't populated by the trigger.** The
   `reject_missing_source_query` rule will fire-default for any
   candidate without source_query. The trigger payload (from
   task-completed signal) needs to provide it OR the digest LLM
   needs to fill it from session context. Today vars_schema
   declares `source_session` and `task_kind` but not `source_query`.
   Without source_query population, every auto-digest run rejects
   100%. Verify: trace the routing packet → arc-vars path; if
   source_query isn't there, document the gap and either:
   - Drop the source_query requirement from entry-quality (relax to
     "source_session OR source_query OR source_files").
   - Have the digest LLM derive source_query from the session's
     last user message.

3. **Quality gate predicates use dual-path `field` references**
   (`field: source_session` OR `field: vars.candidate.source_session`).
   Verbose; every gate field is checked twice. The dual-path
   suggests the engine's gate-eval-against-vars semantics is
   ambiguous. Either:
   - Document why dual-path is needed (gate sees flat OR
     vars-merged entity depending on context).
   - Standardize on one path; engine extension if needed.
   Subjective; works as-is but reads as defensive.

## Concerns

4. **`auto-digest-arc` declares `digest` actor with
   `durable: false`.** Each ProposeEntries invocation is a fresh
   session. Reasonable for one-shot extraction; flag if the
   digest LLM benefits from cross-task memory (probably not).

5. **The 20 audit cases** mostly test the known-good paths
   (auto-apply / hold / reject by category). They DON'T test:
   - The bro-trust composition (untrusted brofile produces hold
     even if other rules would auto_apply)
   - The edge case where source_files is non-empty AND
     source_session is missing (currently rejects)
   - Daily cap boundary (49 vs 50)
   Add 5 more cases covering these; gate fidelity remains 18/20+.

6. **`task-completed-routing.json` packet** triggers auto-digest.
   But `bro_exec` doesn't currently emit `task-completed` signals
   per the design doc release notes. So the routing packet is dead
   code until that signal lands. Document explicitly; flag for the
   bro_exec change in a future phase.

## G2 fix observations

7. **G2 fix #1 (aggregate produced_by)** — `session_ids` /
   `brofiles` / `arc_thread_ids` collected as Vec across all edges
   in the commit. Multi-session commits surfaced correctly. ✓

8. **G2 fix #2 (append provenance notes)** — `git notes append`
   replaces `add -f`. Multiple exports produce multi-section notes.
   Operator workflow documented. ✓

9. **G2 fix #3 (dedupe provenance imports)** — content-hash check
   at append time prevents sidecar growth on re-import. ✓

10. **G2 fix #4 (document notes namespace)** — comment added
    listing `provenance` + reserved kinds. ✓

## Nits

11. **`hold_untrusted_brofile` rule uses `Apply{packet=bro-trust,
    expect=trusted}` then negates via `Not`.** Reads correctly:
    "if not trusted, hold." Could be more direct as `Apply{...,
    expect=[observe, quarantine]}` with `consequent: hold_for_review`.
    Subjective.

12. **Digest-extractor brofile uses `effort: medium`**. For
    extraction quality, `high` might be worth it. Operator-tunable
    via the brofile JSON.

13. **20 audit cases** include 5 hold_for_review (rendered) cases
    that only differ by `category`/`scope`. Could parameterize the
    fixture to reduce repetition. Subjective.
