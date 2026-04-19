# Design packets — ensemble design-iteration via rule-packets

A design packet encodes the criteria for evaluating DESIGN PROPOSALS against a shared set of standards. The workflow is specifically orchestrator-led ensemble iteration: bros propose design variants, each proposal becomes an entity, the packet classifies each proposal against the criteria, the orchestrator sorts by verdict profile.

See `sm-rule-packets` for the universal mechanism. This runbook is the design-iteration instance and covers the workflow pattern, which is the non-obvious part.

## Lattice

`["blocker", "concern", "suggestion", "advantage", "neutral"]` — highest priority first.

- **blocker** — design violates a hard invariant. Proposal must be revised or rejected.
- **concern** — design has a real issue (cost, risk, coupling) that needs addressing but isn't fatal.
- **suggestion** — improvement opportunity worth considering before the proposal lands.
- **advantage** — positive signal; the design handles something well.
- **neutral** — catchall when no other rule fired; "no strong signal either way."

Verdict aggregation: any blocker → verdict = blocker. Else any concern → concern. Etc. Proposals with `verdict == neutral` or lots of advantages beat proposals with blockers/concerns.

## Prefix inference

```
block_     → blocker
concern_   → concern
suggest_   → suggestion
advantage_ → advantage
neutral_   → neutral
```

## Workflow

1. **Define criteria packet.** Compile once up front. Rules are the shared design standards (invariants to preserve, regressions to avoid, advantages to reward).
2. **Broadcast design-space prompt to bros.** Each bro returns a proposal as structured JSON with attributes the criteria reference.
3. **Apply the criteria packet to each proposal** in `mode="all"`. Each proposal gets its own findings list + verdict.
4. **Sort proposals** by verdict profile: blocker-count asc, concern-count asc, advantage-count desc. Surviving set is the shortlist.
5. **Iterate.** Pick top 1-2 proposals; have authors revise in response to the flagged findings; re-apply; repeat until no blockers remain.

## Entity shape

Design proposals are the entities. Attributes describe the proposal's observable properties:

```json
{
  "proposal_id": "prop-rustls-migration",
  "author": "claude",
  "approach": "replace openssl with rustls in all crates",
  "breaks_invariant_no_llm_in_receive_path": false,
  "rust_loc_added": 1200,
  "rust_loc_removed": 800,
  "new_deps": 3,
  "removed_deps": 1,
  "reuses_existing_abstractions": true,
  "performance_delta_ms": -15,
  "breaks_api_compat": false,
  "requires_data_migration": false,
  "coverage_delta_pct": 3.5,
  "supersedes": "prop-openssl-upgrade-v1"
}
```

## Canonical packet shape

```json
{
  "domain": "design-iteration/tls-migration",
  "classification_lattice": ["blocker", "concern", "suggestion", "advantage", "neutral"],
  "prefix_inference": {
    "block_": "blocker",
    "concern_": "concern",
    "suggest_": "suggestion",
    "advantage_": "advantage",
    "neutral_": "neutral"
  },
  "rules": [
    {
      "id": "block_breaks_no_llm_invariant",
      "antecedent": {"op": "Eq", "field": "breaks_invariant_no_llm_in_receive_path", "value": true},
      "consequent": "BLOCKER: proposal breaks the no-LLM-in-receive-path invariant"
    },
    {
      "id": "block_breaks_api_compat_without_plan",
      "antecedent": {"op": "All", "args": [
        {"op": "Eq", "field": "breaks_api_compat", "value": true},
        {"op": "IsMissing", "field": "migration_plan"}
      ]},
      "consequent": "BLOCKER: API compat break without migration plan"
    },
    {
      "id": "concern_large_diff",
      "antecedent": {"op": "Gt", "field": "rust_loc_added", "value": 2000},
      "consequent": "CONCERN: large diff (>2k LoC) — hard to review thoroughly"
    },
    {
      "id": "concern_perf_regression",
      "antecedent": {"op": "LtF", "field": "performance_delta_ms", "value": -5},
      "consequent": "CONCERN: performance regression >5ms"
    },
    {
      "id": "advantage_reuses_existing",
      "antecedent": {"op": "Eq", "field": "reuses_existing_abstractions", "value": true},
      "consequent": "ADVANTAGE: reuses existing abstractions"
    },
    {
      "id": "advantage_perf_improvement",
      "antecedent": {"op": "GtF", "field": "performance_delta_ms", "value": 10},
      "consequent": "ADVANTAGE: performance improves by >10ms"
    },
    {
      "id": "suggest_add_migration_test",
      "antecedent": {"op": "Eq", "field": "requires_data_migration", "value": true},
      "consequent": "SUGGESTION: include a migration repro test"
    },
    {
      "id": "neutral_catchall",
      "classification": "neutral",
      "emit": "fallback",
      "antecedent": {"op": "True"},
      "consequent": "NEUTRAL: no strong signal"
    }
  ]
}
```

## Supersession

Design packets naturally compose with bbox's supersession machinery. When a proposal evolves (v2 addresses v1's blockers), both can live in the store as separate entities, and the new proposal can carry a `supersedes` field. The criteria packet is the evaluation function; the proposals are data that supersedes over time.

## Why this works

The flat-independent-rules model of rule-packets maps exactly to the "evaluate each proposal against each criterion independently" structure of design review. Each proposal gets a findings set; the findings aggregate to a verdict; verdicts sort the proposal set.

What this DOESN'T handle well (phase-next):
- Quantified claims across proposals ("every proposal must address concern X")
- Pairwise comparison ("does proposal A dominate proposal B?")
- Multi-attribute optimization ("best Pareto front")

For those, apply the packet to each proposal, then do the pairwise/aggregate logic in caller code with the findings lists as input.
