# Rule-packets — when and how

A rule-packet is a tiny axiomatic theory: named lookup tables + ordered predicate rules + ad-hoc anomalies. Sender extracts the theory once with an LLM; receiver evaluates it deterministically. **No LLM in the receive path** — the evaluator is a pure function of `(packet, entity) → prediction`.

## When to reach for a packet (vs `remember` / `learn` / `decide`)

You have a body of structured observations — an authorization matrix, retry taxonomy, state-transition table, policy grid, access lattice, categorical decision tree — and you suspect a small set of rules generates it. The knowledge tiers capture *statements*; a packet captures a *generating function*. Packets compress 10–50× against the raw observations and, crucially, generalize to entities the sender never saw (add a new role, the rules apply automatically).

## The compile → audit → apply loop

1. **`bbox_compile(domain, rules, rank_table?, threshold_table?)`** — store the theory. Rules are ordered; first-match-wins by default. Put anomalies before general rules.
2. **`bbox_audit(packet_id, dataset)`** — apply the packet to every `{entity, expected}` pair; return fidelity + mismatches. **Run this before trusting predictions.** Fidelity < 1.0 means at least one rule over-generalizes; mismatches identify the exact rules and inputs.
3. **`bbox_apply(packet_id, entity, mode?)`** — evaluate an entity. `mode="first"` (default) returns the first matching rule. `mode="all"` evaluates every rule independently and returns all findings plus an aggregate verdict — use this for review-style workflows where multiple findings should surface in a single pass.

## Predicate AST

**Integer & equality:**
- `Eq{field, value}` — `entity[field] == value`
- `Ge/Gt/Le/Lt{field, value}` — integer comparison

**Float:**
- `GeF/GtF/LeF/LtF{field, value}` — real-valued comparison (coverage %, rates, confidence scores)

**Applicability — prefer the tri-state set for new rules:**
- `KeyExists{field}` — key exists (value may be null)
- `IsNull{field}` — key exists AND value is the JSON null literal (signals "known non-applicable")
- `IsNonNull{field}` — key exists AND value is non-null
- `IsMissing{field}` — key does not exist ("not computed / extractor failed")

**Deprecated — do not use in new packets:**
- `IsPresent{field}` — same as `IsNonNull` but dishonestly named; collapses missing+null
- `IsAbsent{field}` — fires on either missing OR null, so semantically ambiguous; replace with `IsMissing` or `IsNull`

The deprecated predicates still evaluate for backward compat, but the daemon logs a `tracing::warn!` on every use and they'll be removed after phase-3 migration.

**Cross-field comparison (phase-2 — for structural rules):**
- `FieldEq{lhs_field, rhs_field}` — `entity[lhs] == entity[rhs]`
- `FieldGt/Ge/Lt/Le{lhs_field, rhs_field}` — integer cross-field comparison

**Named idioms:**
- `RankGeFieldThreshold{rank_field, threshold_field}` — the auth-style pattern, kept as a named alias (predates FieldGe)

**Logical composition:**
- `All{args: [...]}` — every sub-predicate must hold
- `Any{args: [...]}` — at least one sub-predicate must hold
- `Not{arg: ...}` — negation
- `True` / `False` — constants (`True` as catchall default)

## Severity is first-class

Each rule has a `severity` field: `fail`, `flag`, `manual`, `pass`, or `info` (default). If omitted at compile time, severity is inferred from the id prefix (`fail_*` → Fail, `flag_*` → Flag, `manual_*`/`review_*` → Manual, `pass_*` → Pass). **Explicit severity beats inferred** — `severity: "info"` survives even if the id prefix is `fail_*`.

**Verdict precedence in `apply(mode="all")`:** `Fail > Flag > Manual > Pass > Info`. The aggregate verdict is the maximum severity that fired.

## Firing semantics: Independent vs Fallback

Each rule has an `emit` field: `independent` (default) or `fallback`.

- **Independent** rules fire whenever their antecedent matches. This is the default and covers nearly everything.
- **Fallback** rules fire in `apply(mode="all")` ONLY when no Independent rule fired. This is how catchall PASS rules should work — visible when nothing else has anything to say, invisible when real findings exist.

In `apply(mode="first")`, emit is irrelevant — first-match-wins walks the full rule list.

**Canonical pass-catchall pattern:**
```json
{"id": "pass_all_clean", "severity": "pass", "emit": "fallback",
 "antecedent": {"op": "True"}, "consequent": "PASS"}
```

## Adversarial-review pattern

The packet's rules *are* the review criteria. Compile a packet whose rules encode "what good looks like" for a change (tests pass, no new warnings, no undocumented tools, no readonly-fs assumptions, etc.). Build an entity from the PR's observable properties. Apply the packet in `mode="all"` — every matching rule contributes a finding, severities aggregate to a verdict. Dispatch peer bros in parallel to critique the packet ITSELF ("what rules are missing? what's over-specific? wrong ordering?") — meta-review of the review criteria.

Prefer `mode="all"` for review. `mode="first"` is for classification tasks where a single answer is wanted (retry policy, authorization, state transition).

## Rule ordering

In `mode="first"`, ordering encodes priority — but because severity is now first-class, prefer a tiered approach:

1. Partition by severity tier: FAIL before FLAG before MANUAL before PASS.
2. Within a tier, order by authoring preference (explicit anomalies before generals).
3. Catchall (`True`) rules come last in their tier.

In `mode="all"`, ordering encodes display order only — every matching rule fires regardless of position. That's the semantic to reach for when you want a full review.

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

## Known gaps (phase-3 followups)

- **`bbox_merge`.** Two packets describing the same domain should merge via behavioral equivalence (evaluate rules on a witness set, cluster by output). Not yet implemented — merge remains manual.
- **`bbox_packets` listing/filtering.** No first-class list tool yet; packets can only be fetched by ID.
- **Rule dependency DAG.** Rules can't suppress other rules ("if fail_foo fires, skip flag_bar"). Currently handled by ordering in `mode="first"` or by the aggregate verdict in `mode="all"`, but a proper dependency graph would let authors express "applicable-only-when" cleanly without nesting.
- **Review-specific dimensions.** The MVP AST handles structural predicates but doesn't model domain-specific review categories (security-sensitive file lists, hot-path symbol tables, coverage deltas computed at evaluation time). These live in the entity, authored by the caller.
