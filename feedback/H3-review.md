# H3 + H1 fixes review

Commits `e9cad8f..9258865` (4 H1 fixes + H3 + H3 fix).

## Issues (fix-forward)

1. **Nightly-eval-arc is observable skeleton, not actual runner.**
   Same pattern as schema-migration-arc and project-bootstrap-arc.
   Each node sets a var to `true`. The `Decide` node has
   `gate: domain:eval/drift-policy` but the gate packet runs against
   the workflow's policy entity (`{step, last_verdict, ...}`) — NOT
   against the eval scoreboard from `RunSuite`'s shell call. So the
   gate's branch routing is effectively random / always-default.
   This recurring deviation needs an explicit "what's-shipped vs
   what-the-design-intends" entry in release notes:
   - The shell harness IS the actual runner; nightly-eval-arc just
     records that it ran.
   - When workflow hook ops gain shell-output capture (`op: shell`
     should populate a var with stdout / exit_code), the gate can
     actually classify drift.
   Document in release notes; don't try to fix the engine here.

2. **`run-agentic-eval.sh::run_llm` defaults to `codex exec` with
   `--dangerously-bypass-approvals-and-sandbox`.** Two concerns:
   - Bypass-approvals means the eval LLM can do ANYTHING in the
     repo (write files, run commands, push commits) without
     restriction. For a nightly cron job this is dangerous —
     should at minimum run in a worktree fork, not the main
     checkout.
   - Hardcoded to codex; the prompt's `EVAL_LLM_CMD` env override
     handles this but the default isn't documented.
   Fix: at minimum, document the bypass-approvals risk in the
   harness header and recommend running the harness inside a
   worktree (`git worktree add /tmp/agentic-eval-worktree HEAD`).
   Better: spawn a worktree internally so the LLM can't pollute
   the main checkout.

3. **`score_one`'s synthetic-regression injection is alphabetical.**
   When `EVAL_SYNTHETIC_REGRESSION=1`, the script injects a
   missing-ref into the manifest with the lexicographically-first
   ID (`sorted([manifest["id"]])[0]` reduces to the manifest's own
   ID — wait, that's just `manifest["id"]` unchanged). Looking
   again: `sorted([manifest["id"]])[0]` is just `manifest["id"]`.
   Pointless sort. The intent was probably "inject regression on
   only the alphabetically-first manifest across the suite," but
   each call processes ONE manifest so the comparison is degenerate.
   Either fix to compare against the suite's first ID (read all
   manifest IDs, find the min, inject only when current matches),
   or drop the alphabetical hedge entirely and inject for ALL
   manifests when `EVAL_SYNTHETIC_REGRESSION=1`.

## Concerns

4. **Eval output dir at `eval/eval-output/<TIMESTAMP>/`.** Inside
   the repo. Will accumulate per-run artifacts that shouldn't be
   committed. Add to `.gitignore` if not already; defer cleanup
   policy to the operator (or add `EVAL_RETAIN_RUNS=10` env).

5. **`extract_json` walks brace-balanced substrings to find a
   JSON object.** Fragile — a code fence `{rust\n...}` would be
   mis-extracted as JSON. Robust approach: ask the LLM to emit
   between specific delimiters (`<json>...</json>` or similar)
   and parse only the delimited block.

6. **`failed_path = OUT_ROOT/latest-failed.txt`** for `mode=failed`
   re-runs. Implementation looks ok; flag that the file is written
   by `aggregate` and read by `manifest_list` — a circular write/read
   that's only valid if `aggregate` ran on a previous invocation.
   First-time `mode=failed` against a fresh OUT_ROOT silently
   produces zero queries.

## H1 fix observations

7. **H1 fix #1 (enrich fused features)** — feature lookup now
   walks all fused entity_ids regardless of source. Vector-only
   hits get type-aware rerank multipliers. ✓

8. **H1 fix #2 (load compact labels for fused results)** — labels
   for non-BM25 results now come from `entity_loader::compact_label`
   instead of the raw entity-ref fallback. ✓

9. **H1 fix #3 (cap rerank boost)** — max combined multiplier
   capped per design recommendation. Test should verify the cap
   fires for a fresh UserConfirmed knowledge entry. ✓

10. **H1 fix #4 (vector_weight tuning)** — `vector_weight: Option<f32>`
    param exposed on `bbox_hybrid_search` AND `bbox_discover_seed_entities`.
    Default 0.6; clamp [0.0, 1.0]; `BM25_WEIGHT = 1.0 - vector_weight`.
    Tool description updated. ✓

## Nits

11. **`run-agentic-eval.sh` uses Python heredocs for parsing/scoring
    inline.** Pragmatic but couples the harness to Python 3 being
    available. Daystrom donor's harness was C# binary; bbox's
    shell-script approach is lighter but binds to `python3`,
    `bash`, `jq`. Document the prereqs in the script header.

12. **`drift-policy` packet** lattice is `stable | drift_minor |
    drift_major`. The Decide node's branch only routes
    `stable | drift_minor | drift_major` — but with the workflow-
    not-actually-running-the-gate issue (#1), the routing is moot.

13. **`9258865 phase H3 fix: stabilize eval gate checks`** — codex
    landed a fix-forward commit during the same turn. Look at the
    diff to see what was unstable; if it's the synthetic-regression
    flake from #3 above, may already be addressed.
