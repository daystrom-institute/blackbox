# Toward mechanically complete LLM-authored workflows

## 1. Thesis

The goal is not to make Blackbox a pleasant way to cosplay a Turing
machine. The goal is to make workflow specs a real mechanical
programming surface:

- an LLM or human authors a static JSON workflow once;
- the workflow engine executes it deterministically;
- packets make mechanical decisions over structured state;
- hooks mutate that state through typed, auditable primitives;
- no runtime LLM judgment or shell escape is required for the mechanical
  part of the computation.

Turing completeness is useful here only as a stress test. It asks:

> What is the smallest set of read, write, and control-flow primitives
> that turns workflows from orchestration glue into an authorable
> deterministic language?

The current system is close. Workflow control flow already supplies
branching, looping, and bounded composition. Packet predicates already
supply pure decision logic over an `ArcContext` entity. Hook ops already
mutate `vars`. The gap is dynamic indexed access: workflows cannot yet
read or write "the cell at the current position" without falling back to
shell or bespoke code.

## 2. Current expressive layers

### 2.1 Packet predicates

The predicate AST (`src/packets.rs`) is intentionally pure. A predicate
evaluates against one JSON entity and returns a Boolean. It has no
mutation, no I/O, and no unbounded recursion. `Apply` composition is
depth-bounded by `MAX_COMPOSITION_DEPTH`; quantified predicates iterate
over finite arrays.

That is the right shape. Packets should remain decidable decision
procedures. Their job in the workflow language is not to compute by
side effect; it is to classify the current machine state.

What packets currently do well:

- compare scalar fields;
- compose Boolean predicates;
- inspect dotted fields in the flattened `ArcContext`;
- quantify over finite arrays;
- compose packet decisions through bounded `Apply`.

What packets cannot yet express cleanly:

- read a string character at a dynamic index;
- read an array element at a dynamic index;
- read an object property by a dynamic key;
- compare a dynamically addressed value without pre-normalizing it into
  a separate `vars` field.

Those missing reads are the main AST gap.

### 2.2 Workflow control flow

The workflow engine already has the control-flow skeleton of a small
language:

| Feature | Existing mechanism |
|---|---|
| Sequence | node `next` |
| Loop | `goto` back-edge |
| Branch | packet gate plus `branch.cases` |
| Stack-ish composition | inline and referenced subworkflows |
| State | `ArcContext.vars` |
| Typed writes | `vars_schema` checked on writes to declared vars |
| Fuel | `max_steps` |

This is deliberately resource-bounded in the running daemon. A concrete
workflow execution is not literally Turing-complete because the engine
enforces `max_steps`, subworkflow depth limits, finite memory, process
limits, and host resource limits. That is operationally correct.

For language-design purposes, the useful question is fuel-parametric:

> Given enough steps and memory, can a static workflow spec express the
> desired mechanical computation?

That framing keeps the proof language honest while still exposing the
same primitive gaps.

### 2.3 Hook mutation

Hook ops are the deterministic mutation layer. Today they can set,
increment, append, merge, parse JSON, call HTTP/MCP, run shell, and
perform several domain-specific Blackbox operations.

The general-purpose state operations are good but shallow:

- `set_var` writes a whole variable;
- `inc_var` increments a whole integer variable;
- `append_var` appends to a whole array variable;
- `merge_var` merges into a whole object variable.

What is missing is typed mutation inside an aggregate:

- replace one character inside a string;
- replace one element inside an array;
- set one key inside an object;
- update an aggregate slot by arithmetic or expression.

Without those, workflows can carry strings, arrays, and objects as
state, but cannot treat them as addressable memory.

## 3. The real target: mechanical authorability

The target surface should let an LLM author specs like:

- deterministic state machines;
- structured validators;
- retry/escalation policies with memory;
- text and JSON transformation pipelines;
- small interpreters over domain-specific languages;
- corpus maintenance workflows that make repeatable decisions without
  asking an LLM every time.

The important distinction:

- **LLM-driven workflow**: the runtime LLM reads state and decides what
  to do next.
- **LLM-authored workflow**: the LLM writes a deterministic spec; the
  engine executes that spec mechanically after authoring.

Blackbox already supports the first well. The primitives below push it
toward the second.

## 4. Missing read primitives

### 4.1 `StringCharAt`

Read one symbol from a string at a dynamic index.

```json
{
  "op": "StringCharAt",
  "field": "vars.tape",
  "index": "${vars.position}",
  "value": "0"
}
```

Required semantics:

- `field` resolves using the existing packet dotted-path lookup.
- `index` accepts either an integer literal or a template expression
  resolved against the predicate entity.
- Predicate-side template resolution for `index` is new evaluator
  capability. Existing packet field lookup is static; this primitive
  should add a narrow helper for literal-or-`${...}` scalar resolution
  rather than importing the workflow hook templater wholesale.
- Indexing is by Unicode scalar value unless the primitive is explicitly
  named byte-oriented.
- Out-of-range reads should support a configurable blank/default symbol
  for tape-like workflows.

Suggested shape:

```json
{
  "op": "StringCharAt",
  "field": "vars.tape",
  "index": "${vars.position}",
  "value": "_",
  "default": "_"
}
```

The `default` matters. Tape-style computations often read untouched
cells before writing them. A write op that pads on write is not enough;
the read side must also define what an absent cell means.

### 4.2 `ArrayAt`

Read one array element at a dynamic index and compare it.

```json
{
  "op": "ArrayAt",
  "field": "vars.cells",
  "index": "${vars.ptr}",
  "compare": "eq",
  "value": 0,
  "default": 0
}
```

This generalizes the same indexed-read infrastructure from strings to
JSON arrays.

Comparison should reuse the existing comparison vocabulary where
possible:

- `eq`
- `ne`
- `gt`
- `ge`
- `lt`
- `le`
- `in`

For v1, `eq` alone is enough to unlock most branch tables. Richer
comparisons can follow.

### 4.3 `ObjectGet`

Read one object value by a dynamic key and compare it.

```json
{
  "op": "ObjectGet",
  "field": "vars.table",
  "key": "${vars.state}",
  "compare": "eq",
  "value": "halt"
}
```

This is useful for transition maps, lookup tables, and workflow-local
indexes. It also avoids forcing authors to generate giant branch packets
when a table lookup is the clearer representation.

### 4.4 Alternative: `PathAt`

Instead of adding three predicates, the engine could add one generalized
dynamic access predicate:

```json
{
  "op": "PathAt",
  "field": "vars.memory",
  "access": [{ "kind": "index", "value": "${vars.ptr}" }],
  "compare": "eq",
  "value": 0,
  "default": 0
}
```

That is more uniform but heavier for authors. The conservative path is
to add explicit primitives first (`StringCharAt`, `ArrayAt`,
`ObjectGet`) and factor their implementation behind one internal helper.

## 5. Missing write primitives

### 5.1 `string_replace_at`

Write one character inside a string variable.

```json
{
  "op": "string_replace_at",
  "args": {
    "var": "tape",
    "index": "${vars.position}",
    "char": "1",
    "pad": "_"
  }
}
```

Required semantics:

- `var` names a top-level `vars` key.
- `index` is resolved by hook arg templating, preserving integer type
  when the entire string is a template.
- `char` must be exactly one Unicode scalar value unless the op is
  explicitly byte-oriented.
- out-of-range writes pad with `pad` up to the target index.
- the resulting whole string is written back through the normal
  `OpEffect::SetVar` path so `vars_schema` validation and event logging
  remain centralized.

### 5.2 `array_replace_at`

Write one element inside an array variable.

```json
{
  "op": "array_replace_at",
  "args": {
    "var": "cells",
    "index": "${vars.ptr}",
    "value": 42,
    "pad": 0
  }
}
```

This makes arrays usable as workflow-local memory. The value should be
any JSON value, with the final array still checked against the variable
kind if `vars_schema` declares the key as `array`.

### 5.3 `object_set`

Set one property inside an object variable.

```json
{
  "op": "object_set",
  "args": {
    "var": "table",
    "key": "${vars.state}",
    "value": "visited"
  }
}
```

This is the write-side pair to `ObjectGet`. It is useful for memoization,
visited sets, counters by key, and local indexes.

### 5.4 Slot update ops

After indexed reads and writes exist, the next ergonomic gap is
read-modify-write. Without it, authors must encode increments as
several nodes and temporary vars.

Potential v2 ops:

```json
{ "op": "array_inc_at", "args": { "var": "cells", "index": "${vars.ptr}", "by": 1, "default": 0 } }
{ "op": "object_inc", "args": { "var": "counts", "key": "${vars.label}", "by": 1, "default": 0 } }
```

These are not needed for the first completeness milestone, but they are
important for authorability.

## 6. Dynamic addressing rules

The key implementation theme is not "add one Turing-machine op." It is
dynamic addressing.

The engine already has two related resolution systems:

- packet field lookup over a JSON entity;
- workflow hook/template resolution over `ArcContext`.

Those systems are not currently the same thing. Hook args already use
workflow template rendering, but packet predicates currently use static
dotted-path lookup through the predicate entity. Predicate-side
`"${vars.position}"` support should therefore be treated as a new,
narrow evaluator helper for `index` and `key` fields.

The new primitives should define one small, reusable rule:

> Any `index` or `key` field may be a literal JSON value or a whole-string
> template expression like `"${vars.position}"`. Template expressions are
> resolved against the same entity/context the primitive is already
> evaluating.

Avoid resolving templates inside arbitrary dotted path strings as the
first move. This:

```json
{ "field": "vars.cells.${vars.ptr}" }
```

is compact, but it hides dynamic behavior in a string path and makes
validation worse. Prefer explicit access parameters:

```json
{ "field": "vars.cells", "index": "${vars.ptr}" }
```

That keeps specs easier for LLMs to author and easier for validators to
reason about.

## 7. Stress tests

### 7.1 Finite-state machine

Minimum test:

- `vars.state`
- one gate packet per state or one combined gate packet
- branch cases for each classification
- transition nodes that mutate `vars.state`

This should already work. It proves that the existing control-flow
surface can express ordinary deterministic state machines.

### 7.2 Tape machine

Next test:

- `vars.tape: string`
- `vars.position: int`
- `vars.state: string`
- `StringCharAt` reads the current symbol
- `string_replace_at` writes a symbol
- `inc_var` / `inc_var by -1` moves the head
- gate classifications choose transition nodes

This is the smallest useful stress test for dynamic indexed string
access. It should be framed as fuel-parametric: for any halting run, the
workflow should reproduce the same steps when given sufficient
`max_steps`.

### 7.3 Brainfuck interpreter

Brainfuck is a useful later fixture, but it is not the minimal proof
target.

A practical BF interpreter wants:

- program string read at `vars.pc`;
- cell array read at `vars.ptr`;
- cell increment/decrement;
- output append;
- input consumption;
- bracket matching or a precomputed jump table;
- step/fuel handling.

That means BF is a good second milestone after indexed reads/writes and
slot arithmetic exist. It should not be used to argue that two
primitives are sufficient.

### 7.4 Real Blackbox workflow fixture

The most valuable test is not a toy interpreter. It is a deterministic
workflow Blackbox actually wants:

- classify a transcript digest candidate;
- normalize fields through hooks;
- maintain counters or seen sets in `vars`;
- branch through a review/escalation policy;
- emit a structured result with no runtime LLM decision after the
  candidate has been parsed.

This kind of fixture proves the authoring surface matters outside the
computability argument.

## 8. Roadmap

### Phase 1: indexed reads

Add packet predicates:

1. `StringCharAt`
2. `ArrayAt`
3. `ObjectGet`

Implementation notes:

- add enum variants in `Predicate`;
- add validation for single-character string values where applicable;
- share a helper for resolving literal-or-template `index` / `key`;
- keep field lookup static and explicit;
- add tests for dotted fields, array indices, missing fields, bad
  indexes, defaults, and type mismatches.

### Phase 2: indexed writes

Add hook ops:

1. `string_replace_at`
2. `array_replace_at`
3. `object_set`

Implementation notes:

- route all mutations through `OpEffect::SetVar`;
- use existing hook arg rendering so whole-string templates preserve
  numeric values;
- pad strings/arrays only when the op declares a `pad` value;
- fail loudly on negative indexes, non-integer indexes, invalid chars,
  and wrong aggregate types.

### Phase 3: authoring fixtures

Add checked examples:

1. finite-state machine workflow;
2. tape-machine workflow with a small halting computation;
3. deterministic Blackbox maintenance workflow;
4. optional Brainfuck interpreter once slot arithmetic exists.

The goal is not benchmark performance. The goal is to make sure a
workflow author can express the computation naturally in JSON and that
the engine executes it mechanically.

### Phase 4: ergonomics

Add only after the primitive semantics settle:

- `array_inc_at`;
- `object_inc`;
- `length` predicates or hook ops;
- `slice` / `substring`;
- `concat`;
- reusable workflow libraries for common state-machine patterns.

## 9. Success criteria

The workflow language is "mechanically complete enough" when:

- packet gates can branch on dynamically addressed state;
- hook ops can mutate dynamically addressed state;
- authors can build deterministic state machines without shell hooks;
- authors can build small interpreters or validators without runtime LLM
  decisions;
- fixtures prove the behavior under explicit `max_steps` fuel;
- failure modes are typed and auditable rather than hidden in shell
  scripts.

At that point, the interesting claim is not "Blackbox is
Turing-complete." The interesting claim is:

> Blackbox workflows are a deterministic, LLM-authorable programming
> surface for bounded mechanical computation, with enough indexed state
> access to express real interpreters, validators, and state machines.
