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

**Packet composition (phase 5):**
- `Apply{packet_id, expect, entity_map?}` — true iff applying the referenced packet produces a first-match verdict whose classification is in `expect`. Lets a theory depend on another theory — a review packet can compose a `is_breaking` packet; an auth packet can compose a `privileged_role` packet. Use when a concept is worth extracting and reusing across packets.

    **When to extract a sub-packet** (the naming-as-compression heuristic):
    - The same cluster of conditions appears in 2+ rules within a packet — pull it out, `Apply` it by name.
    - The concept has a crisp name your collaborator would recognize without the predicate body — "is_breaking", "is_privileged_role", "is_after_business_hours". If the concept DOESN'T have a natural name, it's probably not ready to extract.
    - The concept is reused across packets in the same domain — an auth matrix's `is_privileged` applies to many resource types; a review rubric's `is_breaking` applies to many PR-triage packets.
    - Rule of thumb: extraction reduces cognitive load when it replaces a restatement. It adds load when it fragments a single, crisp rule across two artifacts. One-off conditions stay inline.
    - `expect` is validated at compile time against the sub-packet's lattice — typos fail fast instead of silently never matching.
    - `entity_map` optionally rebinds outer field names into the sub-packet's schema: `{"role": "actor_role"}` populates the sub's `role` from the outer entity's `actor_role`. Unmapped fields pass through unchanged.
    - Compile-time check: referenced `packet_id` must already exist in the store. Compile the sub-packet first.
    - Runtime: depth-limited at 8 composition levels. Exceeding that returns false with a warning log (not a panic).
    - Cycles are detected by depth limit, not visited-set. A direct self-reference compiles because the new packet's ID isn't known until save — authoring discipline catches those.

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

### Emit × Mode interaction (the one non-obvious bit)

The `emit` field means different things under each mode, and this trips authors up:

- In `mode="first"`, `emit` is **ignored**. First-match-wins. Your fallback rule still has to live at the end of the rule list — ordering is what makes it fire last.
- In `mode="all"`, `emit="independent"` rules fire whenever they match. `emit="fallback"` rules only fire when NO independent rule fired — that's what makes a `pass_all_clean` catchall vanish when real findings surface.

Rule of thumb: if you're writing `mode="first"`, just put fallback-style rules last and don't bother setting `emit`. If you're writing `mode="all"`, mark catchalls as `emit="fallback"` explicitly or they'll fire alongside your real findings.

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

## When the AST can't express your rule

Packets are bounded by the predicate AST. If you want to compile a rule but can't find a predicate that fits ("I need rate-over-time", "I need regex match", "I need recursive set-membership"), **log a gap instead of fighting it**:

```
bbox_packet_gap(
    description="wanted to flag requests exceeding 10/min/user; no rate/time predicate",
    domain="rate-limit",
    attempted_sketch="CountInWindow{path:'requests[*]', window_seconds:60, gt:10}",
    fallback_used="prose rubric in reviewer instructions",
    ast_feature_requested="RateCmp"
)
```

Every gap logged is a vote for what the AST can't yet say. Query the aggregate via `bbox_packet_events(op="gap")`. When the same gap shows up three times from different agents, it's time to add the primitive.

Don't compile a packet that half-works and then paper over the holes with prose in the consequent — that reintroduces drift. Either the predicate captures the decision or the gap goes in the log.

## Operation events

`bbox_compile`, `bbox_apply`, and `bbox_audit` each emit a structured event to a rolling log. Query via `bbox_packet_events`:

- `op="compile"` with `outcome="error"` — authoring failures; filter by `domain` to find patterns.
- `op="apply"` with `outcome="no_match"` — catchall gap in the packet; revise rules.
- `op="audit"` with `outcome="low_fidelity"` — packet drifted from its training data; recompile with updated rules.
- `op="gap"` — everything logged via `bbox_packet_gap`.

Events are newest-first, filter by `op`, `packet_id`, `outcome`, `since`, `limit` (default 50, max 500).

## Phase-next gaps

- `bbox_merge` via behavioral equivalence — merge two packets by clustering rules that fire identically on a witness set
- `bbox_packets` list/filter — discovery tool
- Dotted paths in quantified predicates (`config.rules[*]`)
- Nested `ForAll` — currently rejected in v1 to keep evaluator complexity bounded
- `where` filter on `CountCmp` — "count items matching X"
- Rule dependency DAG beyond Independent/Fallback
- Packet-level automation — `bbox_extract_packet(witness_set, domain)` that dispatches an LLM to author rules from examples
