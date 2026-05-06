# M4 + M2 fixes review

Commits `7528c2e..0ed9565` (4 M2 fixes + M4 contradiction-review).

## Issues (fix-forward)

1. **`team: contradiction-specialists` referenced by the ensemble
   actor but no team-creation step is documented.** The 3 specialist
   brofiles are installed via the F4 catalog, but the team that
   bundles them needs `bro_team(action="create", name="contradiction-specialists",
   members=["contradiction-provenance", "contradiction-lifecycle",
   "contradiction-coherence"])`. Either:
   - Add an install script step alongside the brofile installs.
   - Document in release notes that the operator must create the
     team manually before running the arc.
   - Consider an `examples/agentic-corpus/teams/` directory + an
     install hook in the workflow's setup that ensures the team
     exists.
   Current behavior: arc errors at first ensemble dispatch with
   "team not found" until manual setup. Not a v1 blocker but
   needs documented workflow.

2. **`review-synthesis` packet rules use only `field:
   vars.verdict.verdict`** — single path, no dual-path defensive
   check. This contradicts M3's pattern of `Any{field, vars.candidate.field}`.
   Suggests M2's gate-vars verification (M2 fix #1) confirmed the
   engine DOES merge vars into the gate entity. If true, the M3
   dual-path checks are over-defensive and can be simplified
   (already flagged in M3 review #3). Verify and document the
   gate semantic in release notes ("gate predicates can reference
   workflow vars via `vars.X.Y` directly; flat-entity field paths
   work for entities not exposed via vars").

3. **No automated way to revisit the tier-0 detection threshold.**
   Cosine > 0.85 is hardcoded in the embed_queue.rs detection path
   (the prompt set this from design §12.3). Operator may want to
   tune. Surface as a config knob (`BBOX_TIER0_COSINE_THRESHOLD`
   env or in embed.toml) so it's not a code change to adjust.

## Concerns

4. **`contradiction-review-arc` is the second arc that actually
   dispatches LLMs** (after auto-digest's digest-extractor). All 4
   brofiles are read-only-target territory but don't carry
   `disallow_tools` per M3 fix #1 pattern. Apply the same
   protection: `disallow_tools: ["Edit", "Write", "Bash"]` to
   contradiction-{provenance,lifecycle,coherence,facilitator}.

5. **The `Synthesize` node parses facilitator's structured JSON
   into `vars.verdict`** then the gate runs against
   `vars.verdict.verdict`. The full structured output is preserved.
   Other fields (edge_to_write, winning_claim, evidence,
   missing_evidence, required_human_action) — verify they're
   surfaced in the SurfaceToInbox `hold` path so operators have
   the context.

6. **EdgeIndex projection from KnowledgeEntry.links** — verify the
   project_knowledge_edges from S4 was extended to walk `.links`
   alongside `.supersedes`. Without that, the new Contradicts /
   RelatesTo / TensionWith edges land in the JSON store but don't
   appear in EdgeIndex queries.

## M2 fix observations

7. **M2 fix #1 (verify compaction gate vars)** — integration test
   added that confirms the gate sees workflow vars correctly. ✓
   This unblocks the M3 review #3 simplification.

8. **M2 fix #2 (document compaction markers)** — doc comments on
   QuiesceSearch / SwapAtomic explaining v1 no-op rationale. ✓

9. **M2 fix #3 (document compaction scope)** — release notes
   updated re: single-partition-per-tick. ✓

10. **M2 fix #4 (route skip to terminal)** — Skip node dropped;
    branch default routes directly. Cleaner. ✓

## Nits

11. **`hold_default` in review-synthesis** — fallthrough verdict
    is `hold` (not `no_conflict`). Defensive default for
    missing/invalid facilitator verdict. ✓

12. **`supersession_related` check** before emitting tier-0 signal
    — prevents triggering for known supersession chains where
    contradiction is by-design. ✓

13. **Whiteboard registers operator slot as `agent_name=operator`
    role=operator domain=human-review** — same pattern as the
    keystone whiteboard example. Operator can join board by
    invoking `whiteboard_post(agent_name=operator, ...)`. ✓
