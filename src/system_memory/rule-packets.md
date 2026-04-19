# Rule-packets — universal mechanism

A rule-packet is a tiny axiomatic theory stored in bbox: named lookup tables + ordered predicate rules + ad-hoc anomalies, plus a domain-specific **classification lattice** that declares what values rules can emit and how those values aggregate into a verdict. Sender extracts the theory once with an LLM; receiver evaluates deterministically. **No LLM in the receive path** — the evaluator is a pure function of `(packet, entity) → prediction`.

This runbook is the universal-mechanism reference. For domain-specific usage patterns, see the domain runbooks:

- `sm-review-packets` — code review (lattice: fail/flag/manual/pass/info)
- `sm-auth-packets` — authorization (lattice: deny/allow)
- `sm-design-packets` — design-iteration ensembles (lattice: blocker/concern/suggestion/advantage/neutral)

## When to reach for a packet

You have a body of structured observations — an authorization matrix, retry taxonomy, state-transition table, policy grid, access lattice, review criteria, design-proposal set — and you suspect a small set of rules generates it. The knowledge tiers (`bbox_learn` / `bbox_remember` / `bbox_decide` / `bbox_note`) capture *statements*; a packet captures a *generating function*. Packets compress 10–50× against the raw observations and generalize to entities the sender never saw.

## The compile → audit → apply loop

1. **`bbox_compile(domain, rules, classification_lattice?, prefix_inference?, rank_table?, threshold_table?)`** — store the theory. Rules are ordered; first-match-wins in `mode="first"`. Put anomalies before general rules. Supply a domain-specific lattice and prefix inference map, or omit them to default to the review lattice.
2. **`bbox_audit(packet_id, dataset)`** — apply the packet to every `{entity, expected}` pair; return fidelity + mismatches. **Run this before trusting predictions.**
3. **`bbox_apply(packet_id, entity, mode?)`** — evaluate an entity. `mode="first"` (default) returns the first matching rule. `mode="all"` evaluates every rule independently and returns all findings plus an aggregate verdict.

## Predicate AST (domain-neutral)

**Integer & equality:**
- `Eq{field, value}` — `entity[field] == value`
- `Ge/Gt/Le/Lt{field, value}` — integer comparison

**Float:**
- `GeF/GtF/LeF/LtF{field, value}` — real-valued comparison

**Applicability (tri-state):**
- `KeyExists{field}` — key exists (value may be null)
- `IsNull{field}` — key exists AND value is the JSON null literal ("known non-applicable")
- `IsNonNull{field}` — key exists AND value is non-null
- `IsMissing{field}` — key does not exist ("not computed / extractor failed")

**Cross-field comparison:**
- `FieldEq{lhs_field, rhs_field}` — `entity[lhs] == entity[rhs]`
- `FieldGt/Ge/Lt/Le{lhs_field, rhs_field}` — integer cross-field comparison

**Named idioms:**
- `RankGeFieldThreshold{rank_field, threshold_field}` — auth-style pattern, kept as a named alias

**Logical composition:**
- `All{args: [...]}` / `Any{args: [...]}` / `Not{arg: ...}`
- `True` / `False` — constants

## Classification lattice (the domain layer)

Each rule carries a `classification: String` that must appear in the packet's `classification_lattice` (ordered, highest priority first). Lattice ordering IS the aggregate-verdict precedence in `mode="all"`.

```json
// Review domain (default)
{"classification_lattice": ["fail", "flag", "manual", "pass", "info"]}

// Auth domain
{"classification_lattice": ["deny", "allow"]}

// Retry domain
{"classification_lattice": ["dlq", "fail_fast", "backoff", "retry", "noop"]}

// Design-iteration domain
{"classification_lattice": ["blocker", "concern", "suggestion", "advantage", "neutral"]}
```

## Prefix inference

Each packet can declare a `prefix_inference: {id_prefix → classification}` map. When a rule omits `classification`, the compiler walks the prefix map to find a matching id prefix. **Explicit `classification` in a rule always wins over inference.**

Default review-domain prefix map: `{fail_ → fail, flag_ → flag, manual_/review_ → manual, pass_ → pass}`.

## Firing semantics: Independent vs Fallback

Each rule has an `emit` field: `independent` (default) or `fallback`.

- **Independent** rules fire whenever their antecedent matches.
- **Fallback** rules fire in `apply(mode="all")` ONLY when no Independent rule fired.

**Canonical catchall pattern:**
```json
{"id": "pass_all_clean", "classification": "pass", "emit": "fallback",
 "antecedent": {"op": "True"}, "consequent": "PASS"}
```

## Mode choice

- **`mode="first"`** — classification. Returns the first matching rule. Natural for authorization, retry policy, state transitions.
- **`mode="all"`** — review / multi-finding. Evaluates every rule, returns all findings + aggregate verdict. Natural for code review, design critique, validation checklists.

## Self-audit before trust

After compiling, always `bbox_audit` against source observations. A packet with fidelity < 1.0 is lying to you about its training data. The audit call returns the mismatching rule id.

## When a packet is NOT the right tool

- One-shot facts → `bbox_remember`
- User-stated rules that bind the session → `bbox_learn`
- Commitments with rationale and audit trail → `bbox_decide`
- Conversational observations mid-dispatch → `bbox_note`

Packets are for *structured domains that admit generators*.

## Phase-next gaps

- `bbox_merge` via behavioral equivalence
- `bbox_packets` list/filter
- Quantified collection predicates (`ForAll`, `Exists`, `CountCmp`)
- Multi-finding `bbox_audit` (accept `expected_verdict + expected_rule_ids`)
- Rule dependency DAG beyond Independent/Fallback
