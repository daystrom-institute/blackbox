# Rule Packets

Rule packets are the blackbox classification mechanism — a reusable,
deterministic judge that evaluates any entity against a priority-ordered
rule set. No LLM involved at apply time. Packets are portable: dispatch a
`packet_id` to sub-agents and every one of them produces bit-identical
output.

Packets are the answer to "I'm about to paste this rubric into five
sub-agent prompts." Compile once, dispatch the `packet_id` instead.

## The workflow: compile → audit → apply

### 1. Compile (`bbox_compile`)

Author a packet from a domain tag, a classification lattice (priority
order), and a rules array. Each rule has an `id`, an `antecedent`
(predicate AST), and a `consequent` (the output value when the rule
fires).

Rules are **first-match-wins** — order anomalies before general fallbacks.

```json
bbox_compile(
  domain = "pr-triage",
  classification_lattice = ["fail", "flag", "manual", "pass", "info"],
  rules = [
    {
      "id": "fail_tests",
      "classification": "fail",
      "antecedent": {"op": "Eq", "field": "tests_pass", "value": false},
      "consequent": "REJECT"
    },
    {
      "id": "flag_api_change",
      "classification": "flag",
      "antecedent": {"op": "Eq", "field": "api_surface_changed", "value": true},
      "consequent": "FLAG"
    },
    {
      "id": "pass_default",
      "classification": "pass",
      "emit": "fallback",
      "antecedent": {"op": "True"},
      "consequent": "ACCEPT"
    }
  ]
)
```

Returns a `packet_id` (e.g. `packet-7f01324e`). Store it in your prompt
or dispatch it to sub-agents.

### 2. Audit (`bbox_audit`)

**Always run this after compile.** Validates the packet against a known
dataset of `{entity, expected}` pairs. A fidelity < 1.0 means the packet
is misgeneralizing — catches rule-ordering bugs, over-broad antecedents,
and field-name typos before the packet goes live.

```json
bbox_audit(
  packet_id = "packet-7f01324e",
  dataset = [
    {"entity": {"tests_pass": false, "api_surface_changed": false}, "expected": "REJECT"},
    {"entity": {"tests_pass": true, "api_surface_changed": true}, "expected": "FLAG"},
    {"entity": {"tests_pass": true, "api_surface_changed": false}, "expected": "ACCEPT"}
  ]
)
```

Two audit modes:

| Mode | Use when |
|---|---|
| `first` (default) | Single-rule classification: compare the first match's consequent |
| `all` | Multi-finding review packets: compare aggregate verdict + fired rule-id set |

### 3. Apply (`bbox_apply`)

Evaluate any entity against the packet. No LLM, deterministic.

```json
bbox_apply(packet_id = "packet-7f01324e", entity = {"tests_pass": true, "api_surface_changed": true})
```

Two apply modes:

| Mode | Use when |
|---|---|
| `first` (default) | Classification: return the first matching rule's consequent |
| `all` | Review: return every matching rule + aggregate verdict (Fail > Flag > Manual > Pass > Info) |

## Finding existing packets

Before authoring a new packet, check if one already exists for your domain:

```json
// By domain
bbox_packet_list(domain = "pr-triage", latest_per_domain = true)

// By concept — when you don't know the domain label
bbox_packet_list(query = "breaking", limit = 10)

// Via knowledge search (surfaces packets + system memories)
bbox_knowledge(category = "packet")
```

## Packet composition

Packets can embed other packets via the `Apply` predicate — no re-deriving:

```json
{
  "id": "flag_breaking",
  "antecedent": {
    "op": "Apply",
    "packet_id": "packet-deadbeef",
    "expect": "BREAKING"
  },
  "consequent": "FLAG"
}
```

The `Apply` predicate runs the referenced packet against the same entity
and compares the result to `expect`. This lets you layer concerns:
`pr-triage` delegates "is this breaking?" to `breaking-change-detector`.

## Classification lattice

The `classification_lattice` defines the priority order for aggregate
verdicts. Defaults to `["fail", "flag", "manual", "pass", "info"]`.
Supply a domain-specific lattice for auth, retry, design-iteration, etc.:

| Domain | Lattice |
|---|---|
| Auth | `["deny", "allow"]` |
| Retry | `["dlq", "fail_fast", "backoff", "retry", "noop"]` |
| Design review | `["blocker", "concern", "suggestion", "advantage", "neutral"]` |

## Predicate AST reference

| Predicate | Description |
|---|---|
| `True` | Always matches (use as final fallback) |
| `Eq{field, value}` | Entity field equals value |
| `Neq{field, value}` | Entity field not-equals value |
| `Gt{field, value}` / `Lt{field, value}` | Numeric comparison |
| `Contains{field, value}` | String field contains substring |
| `FieldExists{field}` | Entity has the named field |
| `And{predicates[]}` | All sub-predicates match |
| `Or{predicates[]}` | Any sub-predicate matches |
| `Not{predicate}` | Negate a sub-predicate |
| `Apply{packet_id, expect}` | Delegate to another packet's classification |
| `RankGeFieldThreshold{rank_key, threshold_key}` | Role-rank ≥ resource-threshold (lookup-table driven) |
| `IsAllowed{field, allowed:[]}` | Value is in the allowed set |

## Gap logging

When you want to express a rule the AST can't handle (regex matching,
rate-limit thresholds, temporal constraints), log a gap:

```json
bbox_packet_gap(
  description = "wanted regex matching on log messages; no StringMatches primitive",
  ast_feature_requested = "StringMatches"
)
```

Every gap logged is a vote for prioritizing new AST primitives. Query
gaps later with `bbox_packet_events(op = "gap")`.

## When to use a packet vs. something else

| Symptom | Tool |
|---|---|
| "I'm coordinating sub-agents and pasting the same rubric into each prompt" | `bbox_compile` — compile once, dispatch `packet_id` |
| "I need to classify 100+ entities against the same rubric" | `bbox_apply` — deterministic, no LLM drift |
| "I want a durable decision function that survives system prompt changes" | `bbox_compile` — packets are stored, not inlined in prompts |
| "I just need a one-off prose instruction" | Don't compile — use prose in the prompt |
| "I need to remember a convention, not a classification" | `bbox_learn` or `bbox_remember` |
