# Rule-packets — compile a reusable mechanism from examples

**If your task looks like this, compile a packet:**
- "Here are N labeled examples (PRs, errors, access-decisions, proposals...) — build a mechanism that classifies future cases."
- "Rank these N items against these M criteria."
- "Compress this table into something reusable."
- "Judge future X the same way."

The tool you want is **`bbox_compile`**. Not prose. Not Python scripts. Not `bbox_learn` / `bbox_remember` / `bbox_decide` — those capture statements. A packet captures a **generating function** that other agents (or a deterministic evaluator, with no LLM) can apply to future cases.

**Why this tool exists.** Prose rubrics aren't machine-reusable; pseudocode drifts; per-item hand-judgment doesn't scale. Packets compress 10–50× against the raw observations and generalize to entities you never saw. The evaluator is a pure function of `(packet, entity) → prediction` — deterministic, no LLM in the receive path.

**Pick your domain runbook for worked examples:**

- `sm-review-packets` — code review / PR triage (lattice: fail/flag/manual/pass/info)
- `sm-auth-packets` — authorization / access tables (lattice: deny/allow)
- `sm-design-packets` — ranking proposals / design-iteration (lattice: blocker/concern/suggestion/advantage/neutral)

The rest of this runbook is the universal-mechanism reference: predicate AST, classification lattice, modes, validation.

## When NOT to reach for a packet

- Free-form research / synthesis — prose is the right answer
- One-shot facts the user told you — `bbox_remember` / `bbox_learn`
- Durable commitments with rationale — `bbox_decide`
- Conversational signals mid-dispatch — `bbox_note`
- Data with inherently subjective criteria (poem quality, narrative consistency) where mechanical predicates don't apply — decline or offer `bbox_remember`

Packets are for *structured domains that admit generators*. If priors already produce the correct answer and the task is a one-off, prose may be the right economy.

## The compile → audit → apply loop

1. **`bbox_compile(domain, rules, classification_lattice?, prefix_inference?, rank_table?, threshold_table?)`** — store the theory. Rules are ordered; first-match-wins in `mode="first"`. Put anomalies before general rules. Supply a domain-specific lattice and prefix inference map, or omit them to default to the review lattice.
2. **`bbox_audit(packet_id, dataset, mode?)`** — validate the packet against expected outputs.
   - `mode="first"` (default): dataset is `[{entity, expected}]` — compares single-rule consequent.
   - `mode="all"`: dataset is `[{entity, expected_verdict?, expected_rule_ids?}]` — compares aggregate verdict + fired-rule-id set (order-invariant). Use this for review/design packets where multi-finding shape matters.
   **Run this before trusting predictions.**
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

**Quantified collection predicates (phase 4):**
- `ForAll{path, pred}` — every element at `path` satisfies `pred`. Empty/missing collection is vacuously true.
- `Exists{path, pred}` — some element satisfies. Empty/missing is false (no witness).
- `CountCmp{path, compare, value}` — length of collection at `path` compared with `value`. `compare` is one of `lt/le/eq/ge/gt`. Missing path → count 0.

Path syntax in v1: single field with `[*]` suffix, e.g. `"tools[*]"`. Dotted paths like `"config.rules[*]"` are phase-next; flatten the entity if you need them.

Inside the inner predicate, the sub-entity IS the array element. Object elements are addressable directly by their fields (`IsNonNull{field: "description"}`). Primitive elements (strings, ints, bools) get wrapped as `{"$": value}` — address via the special field `"$"`.

No nested `ForAll` inside `ForAll` in v1.

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

- `bbox_merge` via behavioral equivalence — merge two packets by clustering rules that fire identically on a witness set
- `bbox_packets` list/filter — discovery tool
- Dotted paths in quantified predicates (`config.rules[*]`)
- Nested `ForAll` — currently rejected in v1 to keep evaluator complexity bounded
- `where` filter on `CountCmp` — "count items matching X"
- Rule dependency DAG beyond Independent/Fallback
- Packet-level automation — `bbox_extract_packet(witness_set, domain)` that dispatches an LLM to author rules from examples
