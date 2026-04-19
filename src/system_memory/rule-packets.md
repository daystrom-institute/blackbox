# Rule-packets — when and how

A rule-packet is a tiny axiomatic theory: named lookup tables + ordered predicate rules + ad-hoc anomalies. Sender extracts the theory once with an LLM; receiver evaluates it deterministically. **No LLM in the receive path** — the evaluator is a pure function of `(packet, entity) → prediction`.

## When to reach for a packet (vs `remember` / `learn` / `decide`)

You have a body of structured observations — an authorization matrix, retry taxonomy, state-transition table, policy grid, access lattice, categorical decision tree — and you suspect a small set of rules generates it. The knowledge tiers capture *statements*; a packet captures a *generating function*. Packets compress 10–50× against the raw observations and, crucially, generalize to entities the sender never saw (add a new role, the rules apply automatically).

## The compile → audit → apply loop

1. **`bbox_compile(domain, rules, rank_table?, threshold_table?)`** — store the theory. Rules are ordered; first matching antecedent wins. Put anomalies before general rules. Predicate AST: `Eq{field,value}`, `Ge/Gt/Le/Lt{field,value}`, `RankGeFieldThreshold{rank_field,threshold_field}`, `All{args}`, `Any{args}`, `Not{arg}`, `True`, `False`.
2. **`bbox_audit(packet_id, dataset)`** — apply the packet to every `{entity, expected}` pair; return fidelity + mismatches. **Run this before trusting predictions.** Fidelity < 1.0 means at least one rule over-generalizes; mismatches identify the exact rules and inputs.
3. **`bbox_apply(packet_id, entity)`** — evaluate a single entity; return the first matching rule's consequent + rule_id + confidence.

## Adversarial-review pattern

The packet's rules *are* the review criteria. Compile a packet whose rules encode "what good looks like" for a change (tests pass, no new warnings, no undocumented tools, no readonly-fs assumptions, etc.). Build an entity from the PR's observable properties. Apply the packet — the consequent is the most-severe finding. To surface more, set the triggering fields to their 'healthy' value and re-apply; walk the flag tower. Dispatch peer bros in parallel to critique the packet ITSELF ("what rules are missing? what's over-specific? wrong ordering?") — meta-review of the review criteria.

## Rule ordering

Because the evaluator is first-match-wins, ordering encodes priority. Put stakes ahead of confidence: a 0.6-confidence FAIL should come before a 0.9-confidence FLAG of lesser severity, because FAILs should surface first. Lower-confidence rules placed before higher-confidence ones will shadow them. This is mechanical — not vibes — and worth stating the severity class of each rule in its id prefix (`fail_*`, `flag_*`, `pass_*`, `manual_review_*`) so the ordering is auditable.

## Confidence vs severity

Confidence answers "how sure am I this rule is correct?" (dial: 0.6 = lightly-held heuristic, 1.0 = definitional). Severity lives in the consequent string (FAIL / FLAG / PASS / MANUAL_REVIEW). Don't conflate them in ordering.

## Anomalies first

Always list ad-hoc exceptions before the general rules they override. Rule order matters: `(reader, GET, team) = DENY` (anomaly) must precede `GET = ALLOW` (default) or the anomaly never fires.

## When a packet is NOT the right tool

- One-shot facts → `bbox_remember`
- User-stated rules that bind the session → `bbox_learn`
- Commitments with rationale and audit trail → `bbox_decide`
- Conversational observations mid-dispatch → `bbox_note`

Packets are for *structured domains that admit generators*.

## Self-audit before trust

After compiling, always `bbox_audit` against the source observations. A packet with fidelity < 1.0 is lying to you about its training data and will extrapolate badly. The audit call returns the mismatching rule id, so fixes are targeted.

## Known AST gaps (phase-2 followups)

- **Field-vs-field comparison.** Currently no primitive like `FieldGtField{lhs, rhs}`. Rules that should encode "mcp_tools_added > tool_docs_stanzas_added" must hardcode a constant, which over-fits. Add `FieldGt/FieldGe/FieldLt/FieldLe/FieldEq{lhs_field, rhs_field}` for cross-field predicates.
- **`apply_all` mode.** `bbox_apply` currently returns only the first matching rule (first-match-wins). For review-style use where multiple flags should surface, an "apply every rule independently, return all matches" variant is valuable. The walk-the-tower workaround works but is clunky.
- **`bbox_merge`.** Two packets describing the same domain should merge via behavioral equivalence (evaluate rules on a witness set, cluster by output). Not yet implemented — merge remains manual.
- **`bbox_packets` listing/filtering.** No first-class list tool yet; packets can only be fetched by ID.
