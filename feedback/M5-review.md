# M5 + M3 fixes review

Commits `0747f7b..3b20820` (4 M3 fixes + M5 auto-edge arc).

## Issues (fix-forward)

1. **Auto-edge-arc processes ONE candidate per invocation, not 50.**
   `Setup` extracts up to 50 candidate pairs into `vars.candidate_pairs`,
   but the workflow then operates on `vars.candidate` (singular).
   The 50-candidate batch from extract is never iterated. Either:
   - Workflow needs a `foreach` primitive (tracked in
     `bbox_thread:cba8bfa1`).
   - `extract_candidate_pairs` returns one-at-a-time; arc fires N
     times via cron-inlet to process the full batch.
   - Subworkflow loop pattern (back-edge goto) with retry budget
     bounded to N=50.
   Pick one; current behavior processes 1 per arc tick which is
   too slow for nightly cron coverage.

2. **`votes` parsing from ensemble output.** `ClassifyVote` is an
   ensemble actor with 3 members. Their outputs are concatenated as
   `${ClassifyVote.output}`. The on_exit `parse_json` then runs on
   this concatenation. Two issues:
   - 3 JSON objects concatenated = invalid single JSON; `parse_json`
     fails (unless codex extended parse_json for ensemble-aware
     parsing).
   - The vote-aggregate packet expects `vars.vote_aggregate.{vote1,
     vote2, vote3}` keys — the `aggregate_auto_edge_votes` hook op
     must normalize the ensemble output. Verify the hook op handles
     concatenated outputs OR reads each member's output via a
     dedicated API.

3. **`auto-edge-classifiers` team referenced but no install path
   documented.** Same gap as M4's `contradiction-specialists` team.
   Document operator workflow OR add the team-install script (per
   M4 fix #1 — should be a parallel script `install-teams.sh` that
   creates both teams).

## Concerns

4. **Brofiles don't carry `disallow_tools: ["Edit", "Write", "Bash"]`**
   per the M3 fix #1 pattern. The 6 auto-edge brofiles
   (describe-* + reference-*) are read-only intent but unenforced.
   Apply the same protection. One-line per brofile JSON.

5. **`Aggregate` node has both `gate: domain:auto-edge/vote-aggregate`
   AND `prompt: vote1=...`** — the prompt is rendered as the node
   output AND used as the gate input. But the gate predicates
   reference `vars.vote_aggregate.voteN`, not the prompt string.
   The dual-presence is harmless (prompt gets ignored by gate's
   var-aware predicates, and the prompt is captured as node output
   for downstream use) — but it's confusing. Document or remove
   the prompt.

6. **`extract_candidate_pairs` uses `edge_kinds: ["DESCRIBES",
   "REFERENCES"]`** to scope the scan. Verify the implementation
   correctly filters EdgeIndex AND scans both directions
   (markdown→symbol AND knowledge→file). For DESCRIBES, both
   directions might be valid candidates.

## M3 fix observations

7. **M3 fix #1 (read-only digest)** — `disallow_tools: ["Edit",
   "Write", "Bash"]` added to digest-extractor brofile. Read-only
   by construction. ✓

8. **M3 fix #2 (relax provenance gate)** — single rule rejects
   only when ALL three sources (session, query, files) are absent.
   ✓

9. **M3 fix #3 (standardize gate fields)** — dropped dual-path
   `field` references; uses single `vars.candidate.X` path
   throughout. Confirms M2 fix's gate-vars semantic. ✓

10. **M3 fix #4 (simplify bro-trust composition)** — `Apply{expect=
    [observe, quarantine]}` instead of `Not{Apply{expect=trusted}}`.
    Cleaner. ✓

## Nits

11. **Audit cases** at `eval/audit/auto-edge/{describes,references}.json`
    have 15 rows each per the prompt spec. Verify codex hit the 12/15
    fidelity gate for each. Surface in done note if uncertain.

12. **`write_semantic_edge` hook op** writes via `note: vars.vote_aggregate.votes`
    — passes the full vote tally as the edge note. Useful for
    auditability but verbose. Consider a structured note instead of
    raw vote dump.

13. **`SurfaceToInbox` constructs a synthetic candidate** with
    fixed title/content from `vars.candidate`. Inbox surfacing
    should preserve the (entity_a, entity_b, edge_kind) shape so
    operators see what's pending. Verify the formatted note is
    informative, not just `${vars.candidate}` blob-stringified.
