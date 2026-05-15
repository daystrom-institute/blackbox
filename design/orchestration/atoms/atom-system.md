---
title: "Atom System"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - atoms
tags:
  - atoms
  - refactor-tools
date: 2026-05-13
status: "implemented core runtime; design remains the conceptual reference"
brief: "Defines atoms as contracted, discoverable capabilities over bros, workflows, deterministic runners, and adapters."
---

# Atom System

Implementation note: the atom artifact kind, registry/read tools,
`atom_invoke`/`atom_status`/`atom_resume`/`atom_delegate`, deterministic and
adapter runners, workflow-backed atoms, and workflow `atom_bindings` are
implemented. Legacy `bro_agent_*` remains as a compatibility surface while new
public capability work moves to atoms.

Related:
- [Agent System](../agents/agent-system.md) - predecessor design for
  manifest-wrapped brofiles and `bro_agent_*`.
- [Workflow Engine](../../../docs/workflows.md) - current actor / node / subworkflow
  orchestration model.
- [Turing Completeness](../workflows/turing-completeness.md) - workflow specs as a bounded
  deterministic programming surface.
- [Supervision](../supervision/supervision.md) and
  [Supervision Impl](../supervision/supervision-impl.md) - oracle / advisor / evaluation roles
  over running work.
- [Tmux Portal Workflows](../workflows/tmux-portal-workflows.md) and
  [Tmux Portal Impl](../workflows/tmux-portal-workflows-impl.md) - human projection and
  focus for live runs.
- [Phase Decomposer](../phase-decomposer/phase-decomposer.md) and
  [Phase Decomposer Impl](../phase-decomposer/phase-decomposer-impl.md) - workflow-level
  decomposition, foreach dispatch, recomposition, and mediation.
- [Refactor Compound Runs](../../refactor-tools/refactor-compound-runs.md) - deterministic compound
  tool-runner shape.

## Problem

The project has accumulated too many adjacent nouns:

- bros
- agents
- actors
- advisors
- oracles
- teams
- councils
- ensemble nodes
- workflows
- subworkflows
- brofiles
- refactor atoms

Most are individually justified. Together, they create a surface where a
consumer has to understand implementation strata before asking for work. The
distinction between a brofile and an agent manifest is especially hard to
explain from outside the source tree: a brofile is the persona/runtime lens,
while an agent is the typed JSON wrapper around one.

There is also a provider-language collision. "Spin up an agent" means different
things to Claude, Codex, Gemini, and bbox. A caller may interpret it as a
native Claude subagent, a Codex worker, an AgentTool call, or a bbox dispatch.
"Spin up a bro" is intentionally weird, but unambiguous: bbox should create a
runtime worker.

The fix is not to preserve the current `agent` surface. Nobody depends on it.
It is private scaffolding. The correct public model should be atom-first.

## Thesis

**Bros are runtime workers; atoms are discoverable, contracted capabilities.**

An atom is a typed, invocable, composable capability artifact. It declares what
it accepts, what it returns, what behavior it promises, what effects it may
perform, and how it is implemented. Its resolver has a closed implementation
kind:

- `profile` - invokes one bro through a brofile
- `workflow` - invokes a workflow graph
- `deterministic` - invokes a daemon-side deterministic runner
- `adapter` - invokes a daemon-side custom adapter

Large atoms can still be hierarchical. A "compound atom" is a workflow-backed
atom whose workflow invokes other atoms. A "team-backed atom" is a
workflow-backed atom whose workflow performs fanout and aggregation. Those are
useful shapes, not extra resolver kinds.

From the outside, invoking `research-with-adversarial-review` and invoking
`rust-router-extract` should have the same shape: provide typed inputs, receive
a contract-shaped payload, and inspect status/trace/supervision only when
needed.

## Public Vocabulary

### Bro

A **bro** is a runtime worker/session. In code today, real runs land in
`TaskInner` in `src/orchestration/mod.rs`, created through `spawn_task`.

A bro has:

- provider and session id
- task id
- events and status
- cwd / project context
- optional bro label
- transcript cursor/location
- supervision state

`bro_exec`, `bro_resume`, `bro_cancel`, `bro_wait`, and the tmux portal remain
runtime control tools. "Bro" is the imperative runtime noun: spawn one, resume
one, cancel one, focus one.

A profile-backed atom invokes one bro. There is no `implementation.kind="bro"`.
The bro is the live runtime instance; `profile` is the atom resolver kind.

### Brofile / Profile

A **brofile** is a persona and tool-lens profile. It says how a bro should act
when it runs: prompt lens, provider preferences, filters, and ambient behavior.

Brofiles remain independently useful. Not every brofile needs to be an atom,
and not every atom exposes a brofile.

Profile-backed atoms reference brofiles with `implementation.brofile_ref`.
Canonical atoms should not inline brofiles. Named brofiles are easier to audit,
reuse, version, and restrict.

### Atom

An **atom** is the reusable typed capability. It is the thing a caller
discovers, selects, invokes, composes, and version-pins.

An atom manifest answers:

- What is this capability?
- When should a caller use it?
- When should a caller avoid it?
- What inputs does it accept?
- What outputs does it promise?
- What side effects may it perform?
- Which atoms may it invoke?
- What supervision policy applies by default?
- What trace should be retained?
- What implementation kind backs it?
- Where did it come from?

Atoms are standalone artifacts. Workflow files stay workflow files. A
workflow-backed atom points at a workflow; the workflow does not need to carry
the atom contract itself.

### Atom Ref

Atoms and other artifacts are referenced through typed refs:

```text
atom:rust-review@v1
atom:rust-review@latest
workflow:phase-decompose@v1
brofile:rust-refactor-persona@v1
packet:network-policy@v1
team:recompose-council@v1
```

Rules:

- `atom:name@vN` is pinned.
- `atom:name@latest` is explicitly floating.
- bare names are rejected in stored artifacts.
- operator/tool boundaries may accept bare names and resolve them before
  persistence.
- full URI schemes are deferred until cross-daemon namespaces exist.

This keeps version intent round-trippable in JSON instead of hiding it in a
sidecar boolean.

### Subcontract

`_contract` identifies the top-level schema validator. For atoms it is always:

```json
"_contract": "atom/v1"
```

Specialized validation uses an optional typed subcontract:

```json
{
  "_contract": "atom/v1",
  "subcontract": "refactor/v1"
}
```

Subcontracts are closed/versioned overlays. They can enforce additional rules
such as narrow refactor brofile binding or acknowledgement-input constraints.
Tags and categories remain search/ranking metadata; they do not drive
validation.

### AtomBinding

An **AtomBinding** is a workflow-local binding from a node to an atom. It is
the successor to `ActorSpec`, but atom-first.

The atom is the reusable capability. The binding carries local workflow
concerns:

- binding id
- atom ref
- durability
- compaction anchoring
- provider capability requirements
- local budget tightening
- supervision/trace overrides
- portal focus override

Nodes reference bindings by id. Node control flow remains in `NodeSpec`.

### Team / Group

A **team** is a named group used for fanout or rostered work. Teams remain
useful, but a reusable team capability should be packaged behind a
workflow-backed atom:

```json
{
  "implementation": {
    "kind": "workflow",
    "workflow_ref": "workflow:rust-review-council@v1"
  }
}
```

From outside, callers invoke the atom and receive the output contract. The fact
that a team executed internally is trace detail.

### Council / Space

A **council** or **whiteboard** is a coordination space. It is where multiple
runs exchange claims, votes, critiques, and decisions.

Spaces can be used by atom implementations, especially compound workflow-backed
atoms, but they should not become another public invocation noun.

### Oracle / Advisor / Evaluator

These are supervision roles attached to runs, workflows, or atoms:

- an **oracle** is a cheap daemon-side observer that detects suspicion
- an **advisor** is a stronger model asked for judgment
- an **evaluator** scores or validates after the fact

They are not separate public invocation nouns unless an operator is explicitly
configuring supervision.

## Artifact Shape

Canonical atom artifact:

```json
{
  "_contract": "atom/v1",
  "kind": "atom",
  "name": "research-with-adversarial-review",
  "version": 1,
  "manifest": {
    "description": "Research a technical question, challenge the answer, and return a sourced conclusion.",
    "category": "research",
    "tags": ["research", "evidence", "adversarial"],
    "when_to_use": ["The caller needs evidence and explicit uncertainty."],
    "anti_patterns": ["The caller only needs a quick syntax lookup."],
    "inputs": {
      "schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["question"],
        "properties": {
          "question": { "type": "string" },
          "depth": { "enum": ["quick", "normal", "deep"] }
        }
      }
    },
    "outputs": {
      "schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["answer", "evidence", "uncertainty"],
        "properties": {
          "answer": { "type": "string" },
          "evidence": { "type": "array" },
          "uncertainty": { "type": "array" }
        }
      },
      "evidence_density": "high"
    },
    "effects": {
      "writes_files": false,
      "dispatches_runs": 8,
      "max_depth": 3,
      "uses_network": { "gated_by": "packet:research-network-policy@v1" }
    },
    "composition": {
      "may_invoke_atoms": {
        "kind": "allowed",
        "atoms": [
          "atom:research-scout@v1",
          "atom:adversarial-reviewer@v1",
          "atom:synthesizer@v1"
        ]
      }
    },
    "implementation": {
      "kind": "workflow",
      "workflow_ref": "workflow:research-adversarial-review@v2"
    },
    "supervision": {
      "oracle": "default",
      "advisor": "on_alert"
    },
    "trace": {
      "retain": "summary",
      "portal_focus": "on_request"
    },
    "cost_class": "expensive",
    "provenance": {
      "kind": "hand_authored",
      "author": "user"
    }
  }
}
```

The manifest names the contract first and the implementation second.

## Implementation Kinds

`implementation` is a closed tagged union.

### `profile`

Profile-backed atoms invoke one bro through a brofile:

```json
{
  "implementation": {
    "kind": "profile",
    "brofile_ref": "brofile:rust-refactor-persona@v1"
  }
}
```

This is the LLM-specialist case. The resolver expands inputs into a prompt,
applies the brofile lens/tool filters, creates a bro run, and returns an
atom-level handle.

### `workflow`

Workflow-backed atoms package a graph behind one invocation contract:

```json
{
  "implementation": {
    "kind": "workflow",
    "workflow_ref": "workflow:phase-decompose@v1"
  }
}
```

The workflow may contain bindings, nodes, loops, branches, joins, teams,
councils, waits, foreach dispatch, and private subworkflows. The workflow file
does not carry the atom contract in v1.

This is how atoms subsume subworkflows at the reusable capability boundary:
a subworkflow that is only private to one workflow stays a workflow; a
subworkflow meant to be invoked as a capability gets a standalone atom wrapper.

### `deterministic`

Some atoms do not need an LLM. Deterministic atoms run bounded daemon-side
mechanics:

```json
{
  "implementation": {
    "kind": "deterministic",
    "runner": "refactor-plan-validate"
  }
}
```

Examples:

- refactor primitive planners/validators
- compound refactor run validators
- packet evaluators
- lint/format/test gates
- artifact install/supersede actions

v1 ships a closed in-daemon runner registry. Third-party registration is
deferred.

### `adapter`

Adapter-backed atoms delegate to custom daemon code:

```json
{
  "implementation": {
    "kind": "adapter",
    "adapter_name": "badgey"
  }
}
```

Adapters are for capabilities that cannot be cleanly expressed as one brofile,
one workflow, or one deterministic runner. Adapters declare input/output
schemas, handle shape, resumability, and observed effects.

v1 ships a closed in-daemon adapter registry. Third-party registration is
deferred.

## Contract Sub-Schemas

### Inputs And Outputs

Use JSON Schema 2020-12:

```json
{
  "inputs": { "schema": { "$schema": "https://json-schema.org/draft/2020-12/schema" } },
  "outputs": { "schema": { "$schema": "https://json-schema.org/draft/2020-12/schema" } }
}
```

Validation is structural. Atom-to-atom dataflow type checking is out of scope
for v1.

### Effects

Effects are contractual and machine-checked before and during invocation:

```json
{
  "effects": {
    "writes_files": false,
    "dispatches_runs": 0,
    "max_depth": 0,
    "uses_network": false
  }
}
```

Accepted v1 shapes:

- `writes_files`: `false`, `true`, or `{ "scoped": ["path/glob"] }`
- `dispatches_runs`: `0`, bounded integer, or `"unbounded"`
- `max_depth`: `0`, bounded integer, or `"unbounded"`
- `uses_network`: `false`, `true`, or `{ "gated_by": "packet:policy@v1" }`

`atom_invoke` evaluates effects against the atom contract, binding-local
limits, invocation limits, and parent remaining budget.

### Composition

Composition is intentionally minimal in atom v1:

```json
{
  "composition": {
    "may_invoke_atoms": { "kind": "none" }
  }
}
```

`may_invoke_atoms` is a tagged enum:

```json
{ "kind": "none" }
{ "kind": "any" }
{ "kind": "allowed", "atoms": ["atom:x@v1", "atom:y@latest"] }
```

Do not add `parallel_safe`, `chainable_after`, `chainable_before`, or
`fan_out_aggregator` to atom v1. Those fields overclaim what an atom contract
can know. Parallelism and conflict handling depend on workflow DAGs, file
scopes, downstream consumers, and recomposition policy.

### Supervision

Atom manifests declare default supervision:

```json
{
  "supervision": {
    "oracle": "default",
    "advisor": "on_alert"
  }
}
```

V1 values:

- `oracle`: `"none"`, `"default"`, or an atom ref
- `advisor`: `"none"`, `"on_alert"`, `"always"`, or an atom ref

`"default"` resolves through daemon config, such as
`BBOX_DEFAULT_ORACLE_ATOM`. If unset, it degrades to `"none"` with a warning.

No cross-invocation inheritance. Bindings and invocation limits may tighten
supervision, but should not silently weaken required supervision.

### Trace

Atom manifests declare trace retention:

```json
{
  "trace": {
    "retain": "summary",
    "portal_focus": "on_request"
  }
}
```

V1 values:

- `retain`: `"none"`, `"summary"`, `"full"`
- `portal_focus`: `"never"`, `"on_request"`, `"on_attention"`, `"always"`

Full provider transcripts remain provider/run traces. Atom trace summary is a
normalized indexable envelope.

## AtomBinding And Workflow Integration

Current workflow `ActorSpec` is the donor shape, but the first-class model is
`AtomBinding`.

Candidate Rust shape:

```rust
pub struct AtomBinding {
    pub id: String,
    pub atom_ref: AtomRef,
    pub durable: bool,
    pub compaction_anchor: bool,
    pub capability_requires: Vec<Capability>,
    pub limits: Option<BindingLimits>,
    pub supervision_override: Option<SupervisionPolicy>,
    pub trace_override: Option<TracePolicy>,
    pub portal: Option<PortalBindingOverride>,
}

pub struct BindingLimits {
    pub child_budget: Option<u32>,
    pub depth_budget: Option<u32>,
}
```

Important boundaries:

- provider capabilities stay in `capability_requires`
- filesystem/network are atom effects, not provider capabilities
- durability and compaction anchoring are workflow-local concerns
- binding limits can only tighten contract limits
- node control flow stays on `NodeSpec`

`NodeSpec` should reference a binding by id rather than carrying brofile/team
dispatch fields directly. Fields such as prompt, gate, retry, wait, foreach,
matrix, and transition remain node concerns.

## Budgets And Child Invocation

Every invocation computes effective limits:

```text
effective_limit = min(contract, binding, invocation, parent_remaining)
```

The contract declares upper bounds:

- `effects.dispatches_runs`
- `effects.max_depth`
- `composition.may_invoke_atoms`

Bindings and invocation parameters may lower those bounds. Parent invocations
pass remaining budget and depth to children. Exhaustion returns structured
errors such as `budget_exhausted` or `depth_exhausted`.

This is distinct from the raw recursion guard. `atom_invoke` is a policy-gated
capability call; raw `bro_exec`/`bro_resume` are runtime control and remain
denied inside spawned bros unless explicitly escaped.

## Run Handles And Resume

`atom_invoke` returns an atom-level handle. The stable identity is
`invocation_id`, not provider `session_id`, task id, or workflow arc id.

Handle variants:

```rust
pub struct AtomRunHandle {
    pub invocation_id: String,
    pub atom_ref: AtomRef,
    pub parent_invocation_id: Option<String>,
    pub owners: Vec<OwnerRef>,
    pub kind: AtomRunHandleKind,
    pub created_at: u64,
}

pub enum AtomRunHandleKind {
    Profile { provider: String, session_id: String, project_dir: Option<String>, task_id: String },
    Workflow { workflow_ref: WorkflowRef, arc_id: String, root_task_id: Option<String> },
    Deterministic { runner: String },
    Adapter { adapter_name: String, adapter_handle: serde_json::Value },
}
```

Ownership rules:

- creator becomes initial owner
- nested invocations are owned by parent invocation
- operator invocations are owned by `operator:<account>`
- only owners may call `atom_status` or `atom_resume`
- `atom_delegate(handle, to=<invocation_id>)` grants ownership
- v1 does not need revocation
- deterministic atoms usually return `not_resumable`
- adapters declare resumability

## Trace Summary

`atom_status` returns a normalized summary:

```json
{
  "invocation_id": "01H...",
  "parent_invocation_id": null,
  "atom_ref": "atom:research-review@v1",
  "implementation_kind": "workflow",
  "state": "succeeded",
  "started_at": "2026-05-13T00:00:00Z",
  "ended_at": "2026-05-13T00:01:00Z",
  "input_digest": "sha256:...",
  "output_digest": "sha256:...",
  "output_shape": {
    "valid": true,
    "schema_ref": "outputs.schema",
    "errors": []
  },
  "summary": "short result or failure summary",
  "decision_points": [
    {
      "at": "2026-05-13T00:00:30Z",
      "kind": "gate",
      "verdict": "continue",
      "reason": "output matched schema"
    }
  ],
  "children": [
    {
      "invocation_id": "01H...",
      "atom_ref": "atom:research-scout@v1",
      "state": "succeeded"
    }
  ],
  "effects_observed": {
    "writes_files": false,
    "dispatches_runs": 2,
    "uses_network": true,
    "violations": []
  },
  "cost": {
    "input_tokens": 0,
    "output_tokens": 0,
    "dispatched_runs": 2,
    "wall_time_ms": 0
  },
  "artifacts": [
    { "kind": "note", "ref": "note:..." }
  ],
  "errors": []
}
```

States:

- `queued`
- `running`
- `succeeded`
- `failed`
- `cancelled`
- `timed_out`
- `expired`

The trace summary is an atom-level audit and navigation envelope. It does not
replace provider transcripts, workflow event streams, or detailed tool logs.

## Phase-Decomposer Alignment

Atom v1 should not solve parallel safety. The phase-decomposer design already
places parallelism and conflict management where they belong:

- workflow DAG construction
- `foreach` batching
- supervised subworkflows
- collected outcomes
- recomposition council
- mediation and remediation packets

Whether two invocations can run concurrently depends on predicted writes,
sub-unit dependencies, downstream merge order, and recomposition strategy. That
is workflow-level knowledge, not an atom-level truth.

For v1, atoms declare whether they may dispatch children and which children are
allowed. Workflows decide how to schedule, batch, collect, mediate, and retry.

## Surface Design

### Atom Capability Plane

Read/discovery:

- `atom_list`
- `atom_get`
- `atom_describe`
- `atom_search`

Invocation/status:

- `atom_invoke`
- `atom_status`
- `atom_resume`
- `atom_delegate`

Classification:

- default allowed: read/discovery tools
- policy gated: `atom_invoke`
- ownership gated: `atom_status`, `atom_resume`, `atom_delegate`

Spawned bros may call safe atom reads. They may call `atom_invoke` only through
policy gates. Raw runtime control remains denied unless explicitly escaped.

### Bro Runtime Control Plane

Current runtime/control tools remain bro-named:

- `bro_exec`
- `bro_resume`
- `bro_status`
- `bro_cancel`
- `bro_wait`
- `bro_when_all`
- `bro_when_any`
- `bro_broadcast`
- `bro_dashboard`
- `bro_prune`

Portal-oriented tools such as `bro_focus` and `bro_tail` are proposed/portal
layer concerns, not atom schema requirements.

## Relationship To Current Code

### `TaskInner` Remains The Run Substrate

No new provider lifecycle engine is required. `TaskInner` and `spawn_task`
remain the substrate for provider process lifecycle, events, transcripts,
labels, supervision snapshots, and tmux handles.

Atoms invoke runs. Runs remain bros.

### `AgentManifest` Becomes `AtomManifest`

The current agent code is private scaffolding. It contains useful donor pieces:

- description and selection cues
- input/output schema slots
- composition metadata
- provenance
- brofile binding
- embedding data
- dispatch adapter concept

But the public artifact becomes `kind="atom"`, public tools become `atom_*`,
and the implementation type should become `AtomManifest`.

### Workflow Files Stay Workflow Files

Workflow-backed atoms are standalone atom artifacts pointing at workflow refs.
This avoids two contracts in one file and keeps private subworkflows lightweight.

### Engine Escape Hatches

Workflow hook ops run inside the daemon and can bypass provider-facing MCP
filters. Validation must reject atom-backed workflows whose hooks violate
declared atom effects:

- `effects.dispatches_runs: 0` plus raw `bro_exec`
- `effects.writes_files: false` plus file-writing hook ops
- missing/false network declaration for networked hooks
- child atom calls outside `composition.may_invoke_atoms`

## Versioning

There are two version axes:

- artifact version: `atom:name@v1`
- schema contract: `_contract: "atom/v1"`

Artifact versions move independently of schema versions. A future atom schema
version should require an explicit validator path and may coexist in the same
catalog only when the resolver can validate both.

`atom:name@latest` is explicit floating. Stored artifacts should not use bare
names.

## Non-Goals For V1

- Public compatibility with `kind="agent"` or `bro_agent_*`.
- Inline brofiles in canonical atom manifests.
- Full URI namespaces across daemons.
- Third-party deterministic runner registration.
- Third-party adapter registration.
- Path-aware parallel write conflict checking.
- Atom-to-atom dataflow type checking beyond JSON Schema validation.
- A universal trace format that replaces provider transcripts.
- Resuming non-resumable atoms.
- Treating advisor, oracle, evaluator, council, team, or ensemble as atom
  implementation kinds.

## Design Decisions

- Treat **bro** as runtime worker, not capability artifact.
- Treat **brofile** as persona/tool lens.
- Treat **atom** as the typed, discoverable, invocable capability.
- Use standalone atom artifacts. Keep workflow files pure.
- Use typed refs such as `atom:name@v1` and `atom:name@latest`.
- Use `_contract: "atom/v1"` plus optional typed `subcontract`.
- Keep implementation kinds closed: `profile`, `workflow`, `deterministic`,
  `adapter`.
- Use `AtomBinding` as the workflow-local binding, replacing `ActorSpec`.
- Keep atom composition minimal: `may_invoke_atoms` only.
- Keep parallelism and conflict mediation in workflows/phase-decomposer.
- Expose normalized atom trace summaries without replacing raw transcripts.

## Deferred Questions

- How should third-party deterministic runners and adapters be registered?
- Does `foreach.collect` remain sufficient for aggregator behavior, or should
  workflow nodes get an explicit aggregator field?
- What exact stable trace-summary serialization should be frozen in v1.1 after
  real traces exist?
- How should path-aware parallel write conflict checks work once scoped writes
  are common?
- If standalone atom wrappers become noisy, what catalog hygiene or suggestion
  tooling should reduce sprawl?
