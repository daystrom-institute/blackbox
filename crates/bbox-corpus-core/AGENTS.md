# bbox-corpus-core — corpus contract bottom (refs, records, ranking math)

Pure types and leaf logic the whole corpus stack links. This crate depends on
no other bbox crate; keep it that way — things live HERE precisely so crates
below bbox-indexing (ingest passes, sidecars) can call them.

## Project identity

- `project_id` is a HOST-SCOPED realpath hash, not durable identity. A fresh
  registration of the same repo at another path or host mints a different id.
  Cross-host/cross-checkout durability is `repo_id` (first-commit SHA) plus
  declared aliases. Never persist `project_id` where a repo is expected to
  travel.
- `resolve_base_project_for_scope` lives here (not bbox-indexing) so index
  ingest can stamp docs with it. It is the BROAD read gate: descendants and
  ANY worktree of a registered repo alias to the base. The conservative
  managed write gate stays in bbox-indexing. **Do not unify the two gates** —
  the asymmetry is the product: reads may alias an arbitrary user worktree
  harmlessly; writes into one would scatter repo-owned state.
- `ProjectContext.checkout` is present only when the input sat in a
  non-base checkout; `managed = false` checkouts must never receive
  write-side aliasing.

## Rerank math (search/rerank.rs)

- `DEFAULT_COMBINED_CAP = 1.75` is EMPIRICAL (gap-39b3ce16 sweep), not a
  vibe: the maximum legitimate boost product today is UserConfirmed 1.35 ×
  temporal clamp 1.25 = 1.6875, so any cap ≥ that never binds, and the old
  1.5 throttled real knowledge promotions (MRR 0.158 → 0.175 by lifting it).
  **If you add or rescale any boost signal, re-derive the max product and
  re-sweep** — protocol in search/metrics.rs header, `rerank_cap` operator
  probe on bbox_hybrid_search.
- Boost formulas need a value-at-zero check: `recall_boost` shipped as
  `ln_1p(1 + count)` — ≥ ln 2 at count 0, so the 0.25 cap saturated for every
  entry and the recall-frequency signal was silently dead. Zero input must
  produce zero boost, graded from there.
- search/metrics.rs (MRR, recall@k) exists because eval/check.rs measures
  only oracle coverage (is the ref present at all). Anything rank-shaped —
  cap sweeps, signal tuning — must be scored with these, not the oracle.
