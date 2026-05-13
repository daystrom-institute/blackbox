# Atom System

Date: 2026-05-13
Status: proposal

Related:
- [Agent System](../archive/agent-system.md) - predecessor design for
  manifest-wrapped brofiles and `bro_agent_*`.
- [WORKFLOWS](../../WORKFLOWS.md) - current actor / node / subworkflow
  orchestration model.
- [Turing Completeness](turing-completeness.md) - workflow specs as a bounded
  deterministic programming surface.
- [Supervision](supervision.md) and
  [Supervision Impl](supervision-impl.md) - oracle / advisor / evaluation
  roles over running work.
- [Tmux Portal Workflows](tmux-portal-workflows.md) and
  [Tmux Portal Impl](tmux-portal-workflows-impl.md) - human projection and
  focus for live runs.
- [Phase Decomposer](phase-decomposer.md) and
  [Phase Decomposer Impl](phase-decomposer-impl.md) - compound refactor
  orchestration.
- [Refactor Compound Runs](refactor-compound-runs.md) - deterministic compound
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

Most of these are individually justified. Together, they create a surface where
a consumer has to understand implementation strata before they can ask for work.
The distinction between a brofile and an agent manifest is especially hard to
explain from outside the source tree: a brofile is the persona/runtime lens,
while an agent is the typed JSON artifact that wraps one. That is internally
coherent, but not obvious.

There is also a provider-language collision. "Spin up an agent" means different
things to Claude, Codex, Gemini, and the bbox daemon. A caller may interpret it
as a native Claude subagent, a Codex worker, an AgentTool call, or a bbox
artifact dispatch. "Spin up a bro" is intentionally weird, but it is
unambiguous: the bbox orchestration layer should create a runtime worker.

This doc keeps that useful distinction and changes the compositional artifact
noun:

**Bros are runtime workers; atoms are discoverable, contracted capabilities
implemented by single bros, graphs of bros, deterministic mechanical
computation, or daemon-side adapters.**

## Thesis

An **atom** is a typed, invocable, composable capability artifact. It declares
what it accepts, what it returns, what behavior it promises, and how it may be
composed. Its resolver has a closed implementation kind:

- `profile` - a brofile-backed bro run
- `workflow` - a workflow graph, including ensemble/team/council shapes
- `deterministic` - a packet / hook / refactor runner / mechanical tool
- `adapter` - custom daemon code such as Badgey

Large atoms can still be hierarchical. A "compound atom" is a workflow-backed
atom whose graph invokes other atoms. A "team-backed atom" is a workflow-backed
atom whose graph performs fanout and aggregation. Those are useful shapes, not
extra resolver kinds.

The caller should not need to know which one. From outside, invoking a
`research-with-adversarial-review` atom and invoking a `rust-router-extract`
atom should have the same shape: provide typed inputs, receive a contract-shaped
payload, and inspect trace/supervision only when needed.

This reframes the current agent registry as the first implementation of an
atom registry. The existing `AgentManifest` is already close to an
`AtomManifest`: it has selection cuing, input/output contracts, composition
metadata, provenance, brofile bindings, and dispatch adapters. The main change
is ontology and scope, not throwing away the substrate.

## Public Vocabulary

### Bro

A **bro** is the runtime worker/session concept. In code today, every real run
lands in the same substrate: `TaskInner` in `src/orchestration/mod.rs`, created
through `spawn_task`.

A bro has:

- a provider and session id
- a task id
- events and status
- cwd / project context
- optional bro and agent labels
- transcript cursor/location
- supervision state

`bro_exec`, `bro_resume`, and the tmux portal should continue to speak in bros
and runs. "Bro" is the imperative runtime noun: spawn one, resume one, cancel
one, focus one.

A profile-backed atom (`implementation.kind: "profile"`) is the resolver kind
that runs as a single bro. The bro is the live runtime instance; `profile` is
the resolver kind. There is no `bro` resolver kind.

### Brofile / Profile

A **brofile** is a persona and tool-lens profile. It says how a bro should act
when it runs: prompt lens, provider preferences, filters, and ambient behavior.

Brofiles should remain independently useful. Not every brofile needs to be an
atom, and not every atom needs to expose a brofile directly.

The manifest field should remain `brofile_ref`. Renaming only the field to
`profile_ref` would make the schema disagree with the artifact noun and create
another translation layer.

### Atom

An **atom** is the reusable typed capability. It is the thing a caller discovers,
selects, invokes, composes, and version-pins.

An atom manifest should answer:

- What is this capability?
- When should a caller use it?
- When should a caller avoid it?
- What inputs does it accept?
- What outputs does it promise?
- What side effects may it perform?
- What supervision policy applies by default?
- What implementation kind backs it?
- What trace should be retained?
- Which atoms can it compose with?
- Where did it come from?

Existing `examples/agents/refactor/*.json` already use `_contract:
"refactor-atom/v1"` to name small reusable refactor capabilities. This doc
generalizes that instinct: an atom can be tiny and mechanical, or large and
hierarchical.

### Actor

An **actor** is a workflow-local binding. It is not a global type. Inside a
workflow, an actor says "this node runs this capability with these local
requirements."

Today `ActorSpec` has `kind`, `brofile`, `team`, `durable`,
`compaction_anchor`, and `requires`. Actor kinds are deliberately narrow:
executor and ensemble. Roles like planner, reviewer, advisor, and scout are
prompt/profile choices rather than engine types.

That boundary should hold. If a workflow actor references an atom, the actor is
still only the local binding. The atom is the reusable capability.

### Team / Group

A **team** is a named group of bros/brofiles/atoms used for fanout or rostered
work. It is useful but should not be the default top-level abstraction.

Teams can be packaged behind a workflow-backed atom:

```text
atom: "review-rust-api-change"
implementation:
  kind: "workflow"
  workflow_ref: "rust-review-council-review@v1"
```

From outside, the caller invokes the atom and receives the output contract. The
fact that a team executed internally is trace detail.

### Council / Space

A **council** or **whiteboard** is a coordination space, not a worker type. It
is where multiple runs exchange claims, votes, critiques, and decisions.

Spaces can be used by atom implementations, especially compound atoms, but they
should not become another thing users must choose before they can invoke work.

### Oracle / Advisor / Evaluator

An **oracle** is a cheap daemon-side observer over a run. It detects mechanical
or semantic suspicion and raises a signal.

An **advisor** is a stronger model asked for judgment: continue, escalate,
charter drift, exit met, replace bro, and similar verdicts.

An **evaluator** scores or validates the result after the fact.

These are supervision roles attached to runs, workflows, or atoms. They are not
separate public invocation nouns unless an operator is explicitly configuring
supervision.

## Implementation Kinds

Atoms need a stable external contract and a flexible internal implementation
shape. The resolver should only have four implementation kinds:

```json
{
  "implementation": {
    "kind": "profile"
  }
}
```

Valid `kind` values are `profile`, `workflow`, `deterministic`, and `adapter`.

Other labels such as "team-backed" and "compound" describe common graph
shapes, not extra variants in the registry resolver.

### `profile`

The smallest LLM-backed atom is the current agent path:

```text
atom manifest -> brofile -> bro_exec -> TaskInner
```

This is what `bro_agent_dispatch` does today. It loads an `AgentManifest`,
resolves `brofile_ref` or inline brofile, expands inputs into a prompt, applies
filters and ambient context, then calls `spawn_task`.

In atom language, `bro_agent_dispatch` is an early `atom_invoke` for
profile-backed atoms.

### `workflow`

A workflow-backed atom packages a graph behind a single invocation contract.
The workflow may contain actors, loops, branches, joins, councils, teams, and
subcalls.

This is the important collapse:

**A subworkflow is a workflow-backed atom used from another workflow.**

The engine may keep `subworkflow_ref` internally for compatibility, but the
conceptual public target should become `atom_ref`. If the referenced atom is
implemented by a workflow, the workflow engine calls it. If it is implemented by
a single brofile, the engine dispatches one run. If it is implemented by a
deterministic tool runner, the engine executes that runner.

This avoids two parallel composition systems: one for "agents" and one for
"subworkflows."

Workflow-backed atoms also cover team/ensemble and compound shapes. A team atom
can be a workflow with one fanout node plus an aggregator. A compound atom can
be a workflow that invokes several other atoms and returns one outer contract.
The resolver still only sees `kind: "workflow"`.

### `deterministic`

Some atoms do not need an LLM at all. The turing-completeness workflow design
argues for workflows as a bounded deterministic programming surface: static
spec, typed state, packets, hooks, fuel, and no runtime LLM/shell for
mechanical computation.

That substrate can implement deterministic atoms:

- string/array/object state transforms
- refactor primitive planners
- compound refactor runs
- lint/format/test gates
- artifact install/supersede actions

This matters because "atom" should not mean "LLM prompt." It means reusable
capability with a contract.

### `adapter`

Adapter-backed atoms delegate execution to custom code. Badgey is the obvious
example: consultant-flavored behavior plus proposal stores, action journals,
and distillation machinery.

The existing `dispatch_adapter` field on `AgentManifest` is the right shape.
Atom manifests should preserve it as an escape hatch for capabilities whose
implementation cannot be expressed as one brofile or one workflow.

Adapters must declare whether they are resumable and what handle shape they
return.

## Contract Shape

Atom contracts should be stricter than prompt descriptions and more general
than the current agent schema.

Sketch:

```json
{
  "_contract": "atom/v1",
  "name": "research-with-adversarial-review",
  "version": 1,
  "description": "Research a technical question, challenge the answer, and return a sourced conclusion.",
  "when_to_use": ["The caller needs evidence and explicit uncertainty."],
  "anti_patterns": ["The caller only needs a quick syntax lookup."],
  "inputs": {
    "schema": {
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
      "type": "object",
      "required": ["answer", "evidence", "uncertainty"],
      "properties": {
        "answer": { "type": "string" },
        "evidence": { "type": "array" },
        "uncertainty": { "type": "array" }
      }
    }
  },
  "effects": {
    "writes_files": false,
    "dispatches_runs": "unbounded",
    "uses_network": { "gated_by": "research-network-policy" }
  },
  "implementation": {
    "kind": "workflow",
    "workflow_ref": "research/adversarial-review@v2"
  },
  "supervision": {
    "oracle": "default",
    "advisor": "on_alert"
  },
  "trace": {
    "retain": "summary",
    "portal_focus": "on_request"
  },
  "composition": {
    "parallel_safe": true,
    "chainable_after": [],
    "chainable_before": ["synthesize-decision-brief"]
  },
  "provenance": {
    "kind": "hand_authored"
  }
}
```

The exact schema can be smaller in v1. The important part is that the manifest
names the contract first and the implementation second.

### Contract Sub-schemas

The effect, supervision, and trace fields need closed vocabularies so the
registry can search, rank, audit, and packet-classify atoms without prose
interpretation.

Recommended v1 shapes:

- `effects.uses_network`: `false`, `true`, or `{ "gated_by": "<packet_id>" }`
- `effects.writes_files`: `false`, `true`, or `{ "scoped": ["path/glob"] }`
- `effects.dispatches_runs`: integer hint or `"unbounded"`
- `supervision.oracle`: `"none"`, `"default"`, or an atom/brofile ref
- `supervision.advisor`: `"none"`, `"on_alert"`, `"always"`, or an atom/brofile
  ref
- `trace.retain`: `"none"`, `"summary"`, or `"full"`
- `trace.portal_focus`: `"never"`, `"on_request"`, `"on_attention"`, or
  `"always"`

These fields are declarations, not enforcement by themselves. Enforcement still
belongs to dispatch filters, workflow policy packets, MCP surfaces, and the
runtime. The declaration gives those mechanisms a structured target.

## Relationship To Current Code

### `TaskInner` Remains The Run Substrate

No new runtime engine is required. The existing `spawn_task` substrate is the
right place for provider process lifecycle, events, transcripts, labels,
supervision snapshots, and tmux handles if/when portal integration lands.

Atoms invoke runs. Runs remain bros.

### `AgentManifest` Becomes `AtomManifest`

Current `AgentManifest` already has most required fields:

- `description`
- `when_to_use`
- `anti_patterns`
- `brofile_ref`
- `brofile_inline`
- `filter_overlay`
- `inputs`
- `outputs`
- `composition`
- `cost_class`
- `dispatch_adapter`
- `provenance`
- `embedding`

Rather than delete this, evolve it:

- add `_contract: "atom/v1"`
- keep `brofile_ref` as the profile binding field
- replace `AgentSession` with `AtomRunHandle` or keep `AgentSession` as a
  compatibility alias
- rename user-facing tools from `bro_agent_*` to `atom_*`, with
  `bro_agent_*` as aliases during migration
- allow implementation kinds beyond profile-backed dispatch

Internally, the first implementation can literally reuse
`src/orchestration/agents/*` with renamed types later. The conceptual shift can
land in docs and artifacts before the code moves.

### Workflows Can Self-Declare Atom Contracts

Callable workflows should not require duplicate wrapper files. A workflow JSON
may carry `_contract: "atom/v1"`, `inputs`, `outputs`, `effects`,
`supervision`, and `trace` at the top level. When present, the registry projects
that workflow into the atom registry directly.

This mirrors existing refactor atom precedent: the artifact itself declares its
contract rather than requiring a parallel registry entry.

### Workflow Actors Should Reference Atoms

Current workflow actors can reference brofiles and teams. Adding a third
parallel capability slot would recreate the ambiguity this proposal is trying to
remove. `ActorSpec` should converge on exactly one capability slot:

```json
{
  "id": "review",
  "kind": "executor",
  "atom": "rust-api-review@v1",
  "requires": ["implementation"]
}
```

Existing `brofile` and `team` fields become serde aliases during a migration
window:

- `brofile: "x"` normalizes to a profile-backed atom reference
- `team: "x"` normalizes to a workflow-backed atom reference
- `subworkflow_ref: "x"` normalizes to an atom reference whose implementation is
  `workflow`

After the alias window, workflow actors should have one way to name the
capability they run: `atom`.

`ActorSpec.kind` should also retire after the alias window. `executor` versus
`ensemble` is a dispatch shape, and the atom implementation already determines
whether work runs as one profile-backed bro, a workflow graph, deterministic
computation, or an adapter. Keeping both `kind` and `atom` invites contradictory
specs such as `kind: "ensemble"` with a profile-backed atom.

After migration, an actor should carry workflow-local binding fields such as
`atom`, `durable`, `compaction_anchor`, `requires`, and portal policy. Dispatch
shape belongs behind the atom contract.

The workflow engine resolves `atom` to an implementation:

- profile-backed atom -> dispatch one bro
- workflow-backed atom -> invoke nested workflow
- deterministic atom -> execute tool/packet/hook runner
- adapter-backed atom -> call adapter

This is how atoms subsume subworkflows without forcing all atoms to be
workflows.

### Engine Escape Hatches

Workflow internal hook ops can bypass provider-facing MCP filters because they
execute inside the daemon. In particular, an `mcp_call` hook that directly calls
raw `bro_exec` would bypass both the dispatched-provider `bro_*` guard and
`atom_invoke` contract checks.

Workflow install validation should reject atom-contracted workflows whose hook
ops violate declared effects. Examples:

- `effects.dispatches_runs: 0` plus an `mcp_call` to `bro_exec`
- `effects.writes_files: false` plus file-writing hook ops
- missing network declaration for hooks that require networked tools

Runtime policy packets should also flag these violations if they are introduced
through aliases or older artifacts. The contract should describe what the graph
is allowed to do, including daemon-side hook behavior.

### Teams Become Implementation Detail

The team registry stays useful, but a team that is meant to be invoked by
others should usually be wrapped in an atom manifest. That gives it a stable
input/output contract, versioning, provenance, and selection cuing.

### Supervision Becomes Atom Policy

The supervision docs currently distinguish mechanical detection, oracle
observation, advisor judgment, and evaluation. Atom manifests should configure
defaults:

- no supervision for cheap deterministic atoms
- oracle-only for long-running ordinary LLM atoms
- advisor-on-alert for expensive or risky atoms
- final evaluator for user-facing compound atoms

The runtime attaches those policies to `TaskInner.supervision` and workflow run
state. Users should not have to pick "oracle vs advisor" to invoke normal work.

Authority order matters because supervision already appears in several places.
When an atom invocation declares `supervision`, it is the source of truth.
Workflow declarations such as `policy_packet` or the proposed
`anomaly_packet`, and existing `Team.advisor` state until retired, are fallbacks
only when the atom contract is absent. Conflict order:

```text
atom contract > workflow declaration > team declaration
```

The advisor extraction work in the supervision implementation plan should become
the implementation of `supervision.advisor`, not a new team-singleton lifecycle.

### Portal Focuses Runs, Not Atoms

The tmux portal design should stay run-centered. An atom invocation may produce
one run or 140 runs. The portal should not tile every internal run by default.

Atom trace should expose:

- top-level atom invocation id
- child run ids
- summarized status
- attention signals
- focusable run handles

The operator can focus a bro run when they need to steer or debug. The caller of
the atom receives the contract-shaped result.

## Run Handles And Resume

`atom_invoke` should return an `AtomRunHandle` whose shape depends on the
implementation kind. `atom_resume` dispatches on that handle shape rather
than pretending every atom resumes like a single provider session.

- `profile`: equivalent to today's `AgentSession` handle:
  `{ session_id, provider, project_dir, atom, task_id }`.
- `workflow`: `{ arc_id, workflow_id, root_task_id }`; resume means re-enter the
  workflow/arc at its current durable point, subject to the workflow engine's
  existing parent/child arc chain.
- `deterministic`: normally not resumable. Re-invocation should be idempotent or
  fail fast with a structured reason.
- `adapter`: the adapter declares its handle and resume behavior, or returns
  `not_resumable`.

Compound atoms inherit the `workflow` resume story because compound is a graph
shape, not its own resolver kind.

### Ownership And Delegation

The split-plane model depends on handle ownership being explicit. Otherwise
`atom_resume` becomes raw session control in a different coat.

Every atom run handle should carry:

```json
{
  "invocation_id": "atom-run-...",
  "parent_invocation_id": "atom-run-... or null",
  "owners": ["atom-run-..."]
}
```

Rules:

- a bro owns the atom handles it directly invoked
- a workflow-backed atom owns the child atom handles created by its graph
- `atom_delegate(handle, to=<invocation_id>)` appends a new owner
- `atom_status(handle)` and `atom_resume(handle)` require ownership or explicit
  delegation
- calls by non-owners return a structured `forbidden` error

Ownership is about resume/control authority, not transcript visibility. The
trace layer may expose summaries or citations without delegating the right to
resume a child run.

## Composition Model

Atoms are lego-like in two senses:

1. Small atoms can be assembled into bigger atoms.
2. Bigger atoms can still be invoked as one capability.

Composition should support at least:

- chain: output of A feeds input of B
- fanout: A runs across several variants or providers
- ensemble: N atoms produce independent answers, aggregator merges
- escalation: cheap atom first, expensive atom on low confidence
- critique: producer atom plus adversarial reviewer atom
- repair: failing atom routes to repair atom with trace and provenance
- deterministic loop: workflow/packet state drives repeated atom calls with
  fuel

This largely matches the archived agent-system composition section, but the
target should be atom composition rather than agent composition.

Composition is workflow authoring. A separate `atom_compose` tool would be a
second composition surface unless it simply generated workflows. Use
`bro_orchestrate_author` to draft workflows that compose atoms, install them as
workflows, and invoke them through `atom_invoke` once the workflow declares
`_contract: "atom/v1"`.

## Surface Design

The public surface should split the capability plane from the raw runtime
control plane.

Capability plane:

- `atom_list`
- `atom_describe`
- `atom_search`
- `atom_invoke`
- `atom_resume`
- `atom_status`

Runtime control plane:

Today:

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

Proposed with the tmux portal:

- `bro_focus`
- `bro_tail`

`atom_*` is the normal surface for composable capability use. Bros should be
able to use atoms; otherwise profile-backed atoms cannot replace today's
agent-dispatch patterns and workflows cannot become the spelling for compound
atoms.

The recursion guard should therefore not be a blanket deny on `atom_*`.
Instead:

- default-allowed: `atom_search`, `atom_describe`, `atom_list`, and read-only
  `atom_status` for owned/delegated handles
- policy-gated: `atom_invoke`, checked against the atom's `effects`,
  `composition`, remaining parent budget/depth/fuel, and allow-listed child atom
  refs if present
- ownership-gated: `atom_resume`, allowed only for owned/delegated handles
- default-denied: raw `bro_*` dispatch/control inside spawned bros unless
  recursion is explicitly allowed

This makes `atom_invoke` the safe delegation aperture. A bro may invoke a
specialist/profile-backed atom, a deterministic atom, or a bounded workflow
atom, but it cannot acquire unbounded raw orchestration control merely because
it can see `bro_exec`.

Effect contracts drive the check:

- `effects.dispatches_runs`: `0`, bounded integer, or `"unbounded"`
- `effects.writes_files`
- `effects.uses_network`
- cost class
- allowed child atom refs
- parent invocation budget and depth/fuel

The mechanical guard in `src/orchestration/mcp.rs` should move from prefix-only
policy toward a dispatch-capability classification. Prefixes can remain a
backstop for `bro_*`, but atom recursion safety belongs in `atom_invoke`.

`atom_status(handle)` returns invocation-level state normalized across
implementation kinds:

```json
{
  "state": "running | completed | failed | blocked | not_resumable",
  "started_at": "...",
  "ended_at": null,
  "child_runs": { "running": 2, "completed": 5, "failed": 0 },
  "last_event": null,
  "trace_summary": null
}
```

Profile-backed atoms delegate to the underlying `bro_status`. Workflow-backed
atoms summarize arc state. Deterministic atoms return terminal state
immediately. Adapter-backed atoms implement the read or declare it unsupported.

Current `bro_workflow_*` and `bro_orchestrate_*` tools are capability-shaped
operations living under the historical `bro_*` prefix. As atoms land, workflow
install/list/author/run should migrate toward the atom plane where appropriate:
workflow artifacts that declare `_contract: "atom/v1"` are installed and invoked
as atoms. Keep `bro_workflow_*` and `bro_orchestrate_*` aliases through the same
compatibility window as `bro_agent_*`.

Compatibility:

- keep `bro_agent_list/search/describe/dispatch` as aliases
- mark `agent` as a legacy artifact kind or storage alias
- allow existing agent manifests to install as atoms with a compatibility
  normalizer
- preserve exact version pins
- preserve raw `bro_*` for operator/runtime control

The docs and examples should shift to atoms first. Code renames can follow once
the model stabilizes.

## Migration Path

1. **Docs and examples.** Introduce this doc. Crosslink from the archived agent
   system, workflow docs, supervision docs, and refactor atom examples when
   they are next touched.
2. **Schema compatibility.** Add `_contract: "atom/v1"` support while accepting
   current agent manifests.
3. **Registry projection.** Add atom list/search/describe as a projection over
   the existing agent registry plus any deterministic atom catalog entries.
4. **Invoke path.** Implement `atom_invoke` by reusing
   `bro_agent_dispatch` for profile-backed atoms.
5. **Workflow target.** Allow workflow actors to reference one `atom` capability
   slot. Treat `brofile`, `team`, and `subworkflow_ref` as serde aliases during
   the migration window.
6. **Workflow-backed atoms.** Let workflow artifacts self-declare
   `_contract: "atom/v1"`. At this point `subworkflow_ref` can become a
   compatibility alias for `atom_ref` where the atom implementation kind is
   `workflow`.
7. **Team-shaped atoms.** Convert teamplates/teams meant for external
   invocation into workflow-backed atoms with fanout/aggregation graphs.
8. **Deterministic atoms.** Register refactor primitives and compound runners
   as atoms where useful.
9. **Release N+1 canonicalization.** Read `kind="agent"` artifacts as
   `kind="atom"` with a normalizer that re-emits canonical atom shape on next
   supersede. Keep `bro_agent_*` tool aliases and introduce the canonical
   `atom_*` surface.
10. **Guard migration.** Move recursion policy from prefix-only denial to
    capability classification:
    - default-allowed: `atom_search`, `atom_describe`, `atom_list`
    - read/ownership-gated: `atom_status`
    - policy-gated: `atom_invoke`
    - ownership-gated: `atom_resume`
    - default-denied: raw `bro_*` dispatch/control inside spawned bros unless
      `allow_recursion=true`
    `atom_invoke` enforces effect contracts, child budgets, depth/fuel, and
    ownership/delegation. The classification must fan out through the same
    provider-specific filter paths that currently implement the `bro_*`
    recursion guard.
11. **Release N+2 alias retirement.** Remove `bro_agent_*` aliases and reject
    `kind="agent"` disk artifacts with an error that names the migration command.
12. **Rename internals opportunistically.** Move `orchestration/agents` toward
    `orchestration/atoms` only after behavior is covered by tests.

## Versioning

Atom artifacts have two version axes:

- artifact version: the per-name supersession chain and user-facing pin, such as
  `research-with-adversarial-review@v3`
- contract version: the schema validator and resolver behavior, such as
  `_contract: "atom/v1"`

The artifact version is what callers pin. The contract version controls which
normalizer and validator read the file. `_contract: "atom/v1"` and
`_contract: "atom/v2"` may coexist in the same supersession chain only when v2
is a strict superset. Otherwise the catalog should flag a contract break and
require an explicit operator-approved supersession.

## Non-goals For V1

- Atom-to-atom dataflow type-checking beyond JSON Schema. Composition validation
  is best-effort at install/audit time.
- A universal trace shape that replaces provider event streams. The canonical
  record is still the per-bro transcript plus workflow/arc state.
- Unbounded orchestration through atoms. `atom_*` is the capability surface, but
  `atom_invoke` must enforce effect contracts, delegation budget, depth/fuel,
  and provenance. Raw `bro_*` dispatch remains guarded.
- Resuming non-resumable atoms. Deterministic atoms and adapters that declare
  `not_resumable` should return that explicitly.
- Renaming all internal `agent` modules immediately. Behavior and schema should
  stabilize before code churn.

## Design Decisions

- Keep **bro** as the runtime noun because it avoids provider-native "agent"
  ambiguity.
- Use **atom** as the composable artifact noun because it matches the existing
  refactor atom instinct and the desired hierarchical/lego model.
- Treat **agent** as a compatibility term for profile-backed atoms, not the
  top-level public abstraction.
- Treat **actor** as workflow-local, not a registry object.
- Treat **advisor**, **oracle**, and **evaluator** as supervision roles, not
  first-class invocation targets.
- Treat **team** and **council** as workflow/coordination shapes that can be
  packaged behind atoms, not separate resolver kinds.
- Treat **workflow** as the spelling for compound atom composition.
- Let atoms subsume **subworkflows** conceptually, while keeping old fields as
  compatibility aliases during migration.

## Open Questions

1. Should deterministic atoms share the same manifest schema or use a smaller
   schema with a common envelope?
2. How should atom invocation ids relate to task ids, workflow run ids, and
   provider session ids?
3. Should portal policy allow per-invocation operator overrides on top of atom
   defaults?
