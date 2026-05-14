# runtime allocation tier mapping

Date: 2026-05-14
Status: design proposal

## 1. Problem

Blackbox has several ways to start LLM-backed work:

- ad hoc `bro_exec`
- named brofiles and team members
- registered agents
- workflow actors
- profile-backed atoms

These paths mostly bind runtime identity too early. A brofile, agent manifest,
team member, or atom profile tends to name one provider, model, and effort. That
works while the named provider is healthy, but it makes higher-level workflows
brittle when a provider account is exhausted, temporarily degraded, missing a
capability, or simply the wrong cost band for the current workload.

The desired model is:

```text
caller asks for work intent
  -> allocator resolves eligible provider/account/model/effort lanes
  -> allocator leases one healthy lane from the pool
  -> existing spawn/resume machinery runs the task
```

The underlying abstraction should be a shared runtime allocator used by
brofiles, agents, atoms, workflow actors, and other dispatch surfaces.

## 2. Donor Concepts

Daystrom has the right shape for tier mapping:

- `CapabilityTier` is provider-agnostic intent:
  `economy`, `standard`, `premium`, `frontier`.
- `TierMapping` maps `(provider, tier)` to a concrete `model` plus optional
  `effort`.
- `DispatchConfig` lets callers provide explicit `provider`, `model`, `effort`,
  and/or `tier`.
- `AccountDescriptor` carries account-side constraints:
  `AllowedTiers`, `AllowedModels`, `CapabilityTags`, and max concurrency.
- `ProviderSelection` derives capability requirements from the work kind, then
  narrows the account balancer by tier, provider, and capability tags.

Blackbox already has a richer provider capability enum than Daystrom's current
`StructuredOutput`-only tag set:

- `structured_output`
- `vision`
- `long_context`
- `tool_use`
- `resume`

The allocator should preserve that richer vocabulary and adopt the Daystrom
pattern: work requests describe intent and requirements; providers, models, and
accounts advertise what they can satisfy.

## 3. Goals

- Replace static provider/model/effort binding with late-bound runtime
  allocation where the caller permits it.
- Keep operator pins first-class and fail closed when a pin cannot satisfy the
  request.
- Let atoms, workflows, agents, and brofile-backed ad hoc requests share one pool
  view.
- Resolve tier intent to provider-specific model/effort mappings.
- Treat capability tags as hard eligibility requirements, not scoring hints.
- Derive obvious requirements, especially `structured_output`, from contracts so
  manifest authors do not have to remember every tag manually.
- Preserve existing provider runtimes, prompt/lens/filter machinery, MCP
  filtering, recursion guard behavior, task lifecycle, and resume semantics.
- Make allocation explainable through status and selection traces.

## 4. Non-goals

- No new provider runtime.
- No replacement for brofiles as persona/lens/filter bundles.
- No removal of explicit provider, model, effort, or account pins.
- No claim that all providers expose comparable quota signals.
- No automatic semantic downgrade when a provider lacks a required capability.
- No implementation phase plan in this document.

Current probe reality is asymmetric. Claude, Codex, and Z.AI Coding Plan expose
rolling utilization percentages suitable for direct quota capacity:

- Z.AI Coding Plan: `GET https://api.z.ai/api/monitor/usage/quota/limit` with
  `ANTHROPIC_AUTH_TOKEN` from the selected Claude config dir
  (`~/.claude-zai` by default). `TOKENS_LIMIT number=5 unit=3` is the five-hour
  window; `TOKENS_LIMIT number=1 unit=6` is the weekly/seven-day window.
- DeepSeek: `GET https://api.deepseek.com/user/balance` with
  `ANTHROPIC_AUTH_TOKEN` from the selected Claude config dir (`~/.claude-ds` by
  default) as bearer auth. This returns pay-as-you-go balance availability, not
  rolling-window utilization.
- Inception currently has no quota endpoint in this design; it uses the
  OpenCode-backed active-probe donor path described in
  `design/proposed/acquire-drone.md` §6.

Allocation policy must keep these signals distinct: Z.AI can be quota-probe
confidence, while DeepSeek is balance-derived synthetic capacity.

## 5. Vocabulary

**Runtime lane**

A candidate execution target:

```text
(provider, account, model, effort)
```

The lane also has effective capability tags, health state, quota evidence,
in-flight count, cooldown state, and policy metadata.

**Tier**

Provider-agnostic, operator-defined capability/cost intent. Built-in defaults
may seed conventional names:

```text
economy | standard | premium | frontier
```

A tier is not a model alias and it is not a closed enum. It is an opaque key
resolved through a per-provider mapping. Operators may define domain-specific
keys such as `super-el-cheapo-drones`, `review-deep`, `local-only`, or
`vision-premium`.

**Tier mapping**

Operator-editable configuration that maps `(provider, tier)` to concrete runtime
parameters:

```json
{
  "tiers": {
    "economy": {
      "claude": { "model": "claude-haiku-4-5-20251001", "effort": "low" },
      "codex": { "model": "gpt-5.5-mini", "effort": "low" },
      "glm": { "model": "glm-4.5-air", "effort": "low" },
      "deepseek": { "model": "deepseek-v4-flash", "effort": "low" },
      "gemini": { "model": "gemini-2.5-flash-lite" }
    },
    "standard": {
      "claude": { "model": "claude-sonnet-4-6", "effort": "medium" },
      "codex": { "model": "gpt-5.5", "effort": "medium" },
      "glm": { "model": "glm-5.1", "effort": "medium" },
      "deepseek": { "model": "deepseek-v4-pro", "effort": "medium" },
      "inception": { "model": "inception/mercury-2", "effort": "medium" },
      "gemini": { "model": "gemini-3-flash-preview" }
    },
    "super-el-cheapo-drones": {
      "codex": { "model": "gpt-5.3-codex-spark", "effort": "low", "weight": 1.0 },
      "glm": { "model": "glm-4.5-air", "effort": "low", "weight": 0.8 }
    }
  },
  "tier_ladders": {
    "coding-quality": ["super-el-cheapo-drones", "economy", "standard", "premium", "frontier"],
    "review-cost": ["super-el-cheapo-drones", "standard"]
  }
}
```

The orientation is `tier_key -> provider -> mapping` so user-defined tier keys
remain first-class. Mappings may be intentionally partial. A missing
`(provider, tier_key)` entry means
that provider is not eligible for that tier unless the caller broadens the tier
range to another mapped tier. Effort is nullable because some providers expose
no reasoning-effort control.

Tier mappings are runtime configuration, not compiled constants. Operators
should be able to change provider/model/effort mappings without restarting the
daemon. Allocation should read from the current effective mapping snapshot and
status/trace surfaces should report which mapping version or revision was used.

The mapping example is illustrative. Eligibility still applies after tier
expansion. In current code, only Claude and Codex advertise
`structured_output`; GLM, DeepSeek, Inception, Gemini, and Vibe tier entries are
ineligible for workloads that hard-require structured output until their
capability tags or runtime support change. Tier membership is not a capability
grant.

Tier ordering is not intrinsic. The conventional built-ins may have a default
ladder, but user-defined keys are unordered unless the operator places them in a
named `tier_ladder`.

**Capability tag**

A requirement or advertised capability. Tags are set-valued and additive across
provider, model, account, and runtime surfaces.

**Allocation request**

The normalized request every dispatch surface should reduce to before spawning.
It carries intent, hard requirements, pins, preferences, policy, and context.

**Runtime lease**

The allocator's answer: selected lane, environment/account binding, model/effort,
selection trace id, and continuity metadata needed by resume paths.

**Pool**

A named or inline provider/account candidate set. `pool.name` selects a
configured pool. `pool.providers` narrows by provider. Supplying both means the
intersection. An empty intersection is a hard no-candidates error with a
selection trace; it must not silently widen to the named pool or global default.

## 6. Request Shape

Conceptually, all dynamic dispatch paths should lower to this shape:

```json
{
  "tier": "standard",
  "tier_ladder": "coding-quality",
  "tier_mode": "exact",
  "min_tier": null,
  "max_tier": null,
  "capabilities": ["tool_use"],
  "derived_capabilities": ["structured_output"],
  "durable": true,
  "pool": {
    "name": "coding",
    "providers": ["codex", "claude", "glm"]
  },
  "selection_policy": "availability",
  "pin": {
    "provider": null,
    "account": null,
    "model": null,
    "effort": null,
    "authority": "artifact | operator"
  },
  "prefer": {
    "provider": "claude"
  },
  "project_dir": "/repo/x",
  "caller": {
    "kind": "atom",
    "ref": "atom:java-extract-interface@v2"
  }
}
```

`capabilities` are explicit author/operator requirements.
`derived_capabilities` are computed by the dispatch surface. The allocator uses
their union as the effective hard requirement set.

`tier` is the target tier key. `tier_mode` is `exact`, `at_least`, or `bounded`.
`tier_ladder` names the ordering to use for fallback modes. `min_tier` and
`max_tier` are meaningful only in bounded mode and only inside the named ladder.
When `tier` is absent, the caller-facing dispatch surface should either resolve
a default tier before allocation or intentionally request legacy
provider-default behavior.

`selection_policy` may be a named policy resolved from allocator configuration
or an inline structured policy object. Named policies are hot-reloadable runtime
configuration, like tier mappings.

`pin` fields are hard constraints. `prefer` fields affect scoring only.
`pin.authority` records whether the pin came from an artifact default or an
operator request; operator pins may intentionally override tier mappings, but
never override capability, health, account, or safety constraints.

## 7. Tier Semantics

The allocator should support three tier modes:

- **Exact tier**: only lanes with a mapping for the requested tier key are
  eligible.
- **At least tier**: lanes at the requested tier key or later in the named ladder
  are eligible.
- **Bounded tier**: lanes between `min_tier` and `max_tier` in the named ladder
  are eligible.

Exact tier should be the default for artifact-authored requests because it
preserves the artifact author's named-tier intent. Operator calls may choose a
broader fallback mode when availability matters more than staying on the
requested key.

A provider with no mapping for an eligible tier is excluded for that tier. Missing
mappings are not inferred from provider defaults unless the request has no tier
at all.

Fallback modes require an explicit `tier_ladder`. Without a ladder, user-defined
tier keys have no ordering. A request using `at_least` or `bounded` without a
valid ladder should fail closed with `error.bad_allocation_request`, not silently
downgrade to exact matching.

Tier and capability are orthogonal. A `frontier` tier does not imply `vision`,
`structured_output`, or `resume`; it only selects the provider-specific model
and effort band. Capability tags still have to match.

## 8. Mapping And Account Constraints

Tier mappings should have a single effective view after overlay resolution:

1. built-in provider catalog defaults
2. global allocator configuration
3. project allocator configuration
4. artifact-level runtime defaults
5. caller/operator overrides

Later layers override earlier layers for the request being allocated. Durable
system defaults should live in allocator configuration, while brofiles, agents,
atoms, and workflows may carry local runtime intent.

Global and project allocator configuration should be reloadable while the daemon
is running. A reload may be event-driven, explicit through an ops tool, or both,
but new allocations should not require a daemon restart to pick up tier mapping,
pool membership, provider weights, account limits, or capability-tag edits.
Already-started sessions keep their selected lease for continuity; hot reload
affects subsequent allocations.

Account descriptors should be able to constrain and tag lanes:

```json
{
  "account": "account2",
  "provider": "codex",
  "allowed_tiers": ["economy", "standard", "super-el-cheapo-drones"],
  "allowed_models": ["gpt-5.5-mini", "gpt-5.5"],
  "capabilities": ["structured_output", "tool_use", "resume"],
  "max_concurrent": 1
}
```

Account tags narrow what the lane can satisfy. They do not grant provider or
model abilities that the selected runtime lacks.

## 9. Pins And Preferences

Operators must be able to pin concrete runtime details:

```json
{
  "pin": {
    "provider": "codex",
    "model": "gpt-5.5",
    "effort": "high",
    "account": "account2",
    "authority": "operator"
  }
}
```

Pins narrow eligibility. They do not disable validation.

Examples:

- pinned `provider=gemini` plus required `structured_output` fails if Gemini's
  effective lane lacks `structured_output`.
- pinned `account=account2` fails if that account is exhausted, disabled, over
  concurrency, or lacks the requested tier/model.
- pinned `model=gpt-5.5` bypasses tier-to-model selection for the pinned provider
  but still requires account health and capability coverage.
- pinned `effort=high` overrides tier effort only when the selected provider can
  express that effort.

Pin precedence:

1. `pin.provider` restricts provider candidates.
2. `pin.account` restricts account candidates under the selected provider.
3. `pin.model` overrides tier-to-model resolution, but does not bypass tier
   eligibility unless `tier` is absent or `pin.authority="operator"`.
4. `pin.effort` overrides mapped effort when the provider supports effort.
5. Capability, health, quota, lifecycle, and concurrency checks still apply.

If both `tier` and `pin.model` are present, the trace must say whether the model
was accepted as an operator override or rejected because it is outside the tier's
allowed mapping. Artifact-authored model pins should generally be treated as
part of the artifact's runtime constraints; operator-supplied pins are
authority, but still validated for capability and availability.

Preferences are soft:

```json
{
  "prefer": { "provider": "claude" }
}
```

Preferences influence score but may lose to hard health, quota, capability, or
availability constraints.

Artifact defaults should generally use preferences unless the artifact has a
real semantic dependency on one provider/model. Operator-supplied hard pins
should remain hard.

## 10. Capability Semantics

Capability requirements are fail-closed.

Eligibility must be evaluated before scoring:

1. Resolve candidate lanes.
2. Compute each lane's effective capability tags.
3. Drop lanes missing any required tag.
4. Score only surviving lanes.

Initial tags:

| Tag | Meaning |
|---|---|
| `structured_output` | Native schema-backed or equivalently enforced structured final output. |
| `vision` | Can consume image inputs. |
| `long_context` | Can accept long context at the threshold Blackbox defines for the request. |
| `tool_use` | Can call required tools/MCP surfaces, not just emit text. |
| `resume` | Can continue a provider session reliably. |

Future tags should remain narrow and testable. Examples: `json_mode`,
`mcp_remote`, `shell_tool`, `file_edit`, `low_latency`, `large_output`.

## 11. Derived Capability Requirements

Dispatch surfaces should derive obvious requirements and pass them to the
allocator.

| Condition | Derived tag |
|---|---|
| Output schema requires native/enforced schema compliance | `structured_output` |
| Atom output schema is declared and invocation expects machine-validated output | `structured_output` |
| Workflow node parses strict JSON from an actor as a contract | `structured_output` |
| Request includes image inputs or image file references | `vision` |
| Actor/atom/session is durable or resumable | `resume` |
| MCP/tool access is required by persona, filters, workflow, or atom protocol | `tool_use` |
| Estimated context exceeds normal provider window | `long_context` |

This is the "inherit structured output tag" rule: if an atom, agent, or workflow
node asks Blackbox to enforce or rely on a structured output contract, the
allocator should automatically require `structured_output` unless the caller
explicitly marks the output as best-effort text parsing.

Explicit requirements and derived requirements are both operator-visible in
selection traces.

## 12. Effective Lane Tags

Provider-level capability tags are not enough.

Effective lane tags should be the union/intersection implied by:

- provider capabilities
- model capabilities
- account capabilities
- selected tier mapping
- dispatch/runtime features enabled for that lane
- brofile, agent, atom, or workflow overlays that require a tool surface,
  structured output mode, durable sessions, workspace coercion, or MCP filters

Some tags are naturally provider-wide today, such as basic tool invocation for
providers with stable MCP support. Others are model- or mode-specific:

- `long_context` belongs on the selected model/tier mapping, not on every Claude
  lane forever.
- `vision` may depend on model and input mode.
- `structured_output` may depend on provider support and the exact CLI flag path.
- `resume` may depend on provider plus account-home/session registry behavior.

The design should not bake in provider-only capability checks. Provider-only
checks are acceptable as an initial approximation, but the allocator model must
allow lane-level tags.

## 13. Eligibility

A lane is eligible only when all hard constraints pass:

- provider is in the requested pool, if any
- provider matches any hard provider pin
- account matches any hard account pin
- model and effort match hard pins, if present
- tier mapping exists for the selected tier, unless the request has no tier or an
  operator-pinned model is explicitly authoritative
- account allows the selected tier and model, if it declares restrictions
- effective capability tags cover explicit plus derived requirements
- provider binary/runtime is available
- account credentials are present and not expired
- account is active/selectable, not disabled or quarantined
- account is below max concurrency
- quota is not known exhausted
- cooldown does not hard-exclude the lane

Unknown quota is not the same as zero utilization. Unknown quota should reduce
confidence or score, not falsely outrank quota-aware lanes.

## 14. Scoring

Scoring happens only after eligibility. The allocator should not hard-code one
global load-balancing algorithm beyond the invariants in this document.
Operators should be able to choose or tune a constrained selection policy per
pool, caller class, project, or artifact.

Selection policy is a sum type:

```json
{ "selection_policy": "availability" }
```

resolves a named policy from allocator configuration, while:

```json
{ "selection_policy": { "kind": "score", "score": { "quota_capacity": 0.4 } } }
```

uses an inline policy object for this request. Named policies are preferable for
system and project defaults because they are auditable and hot-reloadable.

Useful score factors:

```text
score = policy_weight
      * provider_preference
      * quota_capacity
      * concurrency_capacity
      * tier_fit
      * reliability
      * recency_spread
      * cost_bias
```

Different caller classes need different policy profiles:

| Policy | Bias |
|---|---|
| `availability` | Maximize chance of starting now. |
| `economy` | Prefer lower cost within hard requirements. |
| `quality` | Prefer higher capability/provider weights. |
| `spread` | Avoid hot accounts and distribute pressure. |
| `sticky` | Prefer the same lane family as related prior work. |
| `deterministic` | Stable selection for tests and reproducible atoms. |
| `round_robin` | Rotate eligible lanes through a stable cursor. |

`tier_fit` is the score component for fallback tier mode. Exact-tier matches
score highest. In `at_least` mode, a later tier in the named ladder can be
selected but may receive a cost penalty unless the policy is `quality`. In
bounded mode, ladder-relative distance from the target tier affects score inside
the admissible range. With exact tier mode, `tier_fit` is binary. Without a
ladder, fallback modes are invalid.

Selection policy should be structured data, not hidden code:

```json
{
  "kind": "score",
  "eligible": [
    "active",
    "has_required_capabilities",
    "below_concurrency",
    "not_exhausted"
  ],
  "score": {
    "provider_preference": 0.25,
    "quota_capacity": 0.35,
    "concurrency_capacity": 0.20,
    "recency_spread": 0.15,
    "tier_fit": 0.05
  },
  "tie_break": ["oldest_selected", "lowest_in_flight", "stable_lane_id"]
}
```

Round-robin is also a policy, not a special dispatch path:

```json
{
  "kind": "round_robin",
  "skip": ["not_eligible", "cooldown", "max_concurrency"],
  "cursor_scope": ["pool", "tier", "capability_set"],
  "tie_break": ["stable_lane_id"]
}
```

The policy language should be expressive enough for filters, weights, fallback
ladders, and tie-breaks, but not arbitrary code. Atomic lease acquisition,
release, cooldown mutation, and runtime observation fusion remain allocator
responsibilities.

General-purpose pooled brofiles should likely default to `availability`.
Profile-backed atoms should default to a conservative policy, probably
`deterministic` or `availability` depending on the atom manifest.
Operator calls may override.

## 15. Rule Packets And Policy Evaluation

This design should reuse the rule-packet/AST substrate instead of inventing a
second routing language inside the allocator.

Rule packets are a good fit for deriving allocation request overlays from caller
context:

```text
caller/work kind/artifact/project/operator context
  -> packet-backed policy evaluation
  -> normalized allocation request
```

Examples:

- output schema exists and is machine-consumed -> add `structured_output`
- input includes image media -> add `vision`
- durable workflow actor -> add `resume`
- cheap atom or general pooled worker -> set `tier = "super-el-cheapo-drones"`
- refactor atom in this project -> set `pool.name = "coding"`
- operator pin -> add hard `pin` fields with `authority = "operator"`
- project policy -> choose `selection_policy = "spread"` or an inline policy
  object

The allocator should consume the normalized request plus a resolved selection
policy. It should not evaluate arbitrary high-level routing rules itself.

Default-tier ownership should also live in this derivation layer. A dispatch
surface can provide a simple default, but system-default rule packets should be
the preferred way to express context-dependent defaults such as "cheap atoms use
`super-el-cheapo-drones`" or "vision requests use `vision-premium`."

The boundary:

```text
rule packets / policy AST:
  derive request overlays, tier keys, pools, capability requirements,
  fallback ladders, selection policy references, and scoring weights

allocator:
  evaluate live pool state, apply hard eligibility, score/rotate candidates,
  atomically issue and release leases, record observations

spawn path:
  apply prompt wrapping, lens, MCP filters, recursion guard, CLI args,
  and provider process lifecycle
```

This preserves Blackbox customizability without making rule packets responsible
for mutating live lease state.

## 16. Caller Integration Model

### Brofiles

Brofiles should remain persona/runtime-shaping artifacts:

- lens
- filters
- tool surface overlays
- workspace coercion
- account defaults, when intentionally pinned
- optional runtime allocation defaults

They should not be the only way to choose provider/model/effort.

A brofile may still pin a provider/model/effort. That pin becomes an allocation
constraint. A brofile may also express softer runtime intent:

```json
{
  "runtime": {
    "tier": "standard",
    "capabilities": ["tool_use"],
    "selection_policy": "availability",
    "prefer": { "provider": "claude" }
  }
}
```

A general pooled worker is just an unpinned brofile with runtime allocation
defaults. It does not need a separate acquisition tool:

```json
{
  "name": "pool-worker",
  "lens": "General-purpose worker persona...",
  "runtime": {
    "tier": "standard",
    "pool": { "name": "coding" },
    "selection_policy": "availability",
    "capabilities": ["tool_use"]
  },
  "coerce_workspace": true
}
```

Allocation chooses only the runtime lane. It does not own prompt wrapping, MCP
filter resolution, recursion guard construction, or provider CLI argument
assembly. Those remain spawn-path responsibilities after the lease is selected.
This keeps allocation from becoming a side door around safety filters.

### Agents

Agent manifests currently use `brofile_ref` or `brofile_inline` as direct
runtime bindings. Under this design:

- agent persona can still come from brofile_ref or inline lens
- runtime allocation can come from agent-level intent
- pins in the brofile or agent manifest narrow allocation
- filter overlays merge as they do today

Adapter-backed agents may either use their adapter-owned lifecycle or request
allocator leases for scout/worker dispatches.

### Workflows

Workflow actors already have `requires`. Those requirements should become
allocator requirements rather than static brofile-provider checks.

Workflow actor dispatch should lower actor configuration to an allocation
request:

```json
{
  "tier": "standard",
  "capabilities": ["structured_output"],
  "durable": true,
  "selection_policy": "availability"
}
```

Compile-time validation can remain useful, but it should answer a routability
question: "does this actor request have any configured lane that could satisfy
it?" Dispatch-time validation still matters because account health changes.

### Atoms

Atoms are the main place where static binding hurts.

Profile-backed atoms currently bind through `implementation.profile.brofile_ref`.
The brofile should continue to provide persona, prompt discipline, and filters,
but the atom should be able to request runtime allocation:

```json
{
  "runtime": {
    "tier": "standard",
    "capabilities": ["structured_output", "tool_use"],
    "selection_policy": "deterministic",
    "pool": {
      "providers": ["codex", "claude", "glm"]
    }
  }
}
```

For prompt-sensitive atoms, hard pins remain available:

```json
{
  "runtime": {
    "pin": {
      "provider": "codex",
      "model": "gpt-5.5",
      "effort": "medium"
    }
  }
}
```

Atom output schemas should derive `structured_output` when invocation expects the
provider to enforce the schema or when the result is machine-consumed without a
human repair loop.

### Raw `bro_exec`

Raw provider mode can remain for backward compatibility:

```text
bro_exec(provider="codex", ...)
```

Semantically, this should be equivalent to an allocation request with
`pin.provider = "codex"` and no tier unless the caller supplies one.

## 17. Resume And Continuity

Allocation is not just startup. Provider sessions live under provider account
homes, and resume must use the same lane that created the session unless the
provider explicitly supports cross-account/session relocation.

`durable=true` means the lease must persist enough continuity metadata for
resume before the caller receives a resumable handle. Non-durable allocations
may still record selection traces and task accounting, but they do not promise a
future resume path.

A durable lease should persist:

```json
{
  "task_id": "...",
  "session_id": "...",
  "provider": "codex",
  "account": "account2",
  "model": "gpt-5.5",
  "effort": "medium",
  "tier": "standard",
  "durable": true,
  "capabilities": ["structured_output", "tool_use", "resume"],
  "project_dir": "/repo/x",
  "cwd": "/repo/x",
  "caller": { "kind": "atom", "ref": "atom:java-extract-interface@v2" }
}
```

The `cwd` field is load-bearing. Resume paths should consult the lease/session
binding first and use the stored account, model, effort, project directory, and
cwd. If the original lane is unhealthy, resume should fail with a precise
diagnostic unless the caller has requested a provider-specific fork/migration
mode. Silent resume on a different account or cwd is unsafe.

## 18. Observability

Every allocation should be explainable.

Selection traces should include:

- normalized request
- packet/policy inputs that produced request overlays
- explicit requirements
- derived requirements
- effective requirement union
- pins and preferences
- tier mode and candidate tiers
- all considered lanes
- per-lane eligibility outcome
- exclusion reasons
- score components for eligible lanes
- selected lane
- health/probe evidence summary
- caller identity

Status views should show the same concepts at pool level:

- configured tier mappings
- providers/accounts/models per tier
- effective capability tags
- active in-flight counts
- quota confidence
- cooldown state
- current exclusions

## 19. Relationship To acquire-drone.md

`design/proposed/acquire-drone.md` is superseded as a dispatch-tool design.
Pooled workers are modeled here as brofiles, agents, atoms, or workflow actors
with runtime allocation defaults, not as a separate `acquire_drone` MCP surface.

What remains useful from that earlier document:

- provider probe taxonomy
- account health and quota confidence fields
- cooldown and runtime observation fusion
- selection traces
- session/account/cwd continuity

What is retracted:

- a first-class `acquire_drone` dispatch tool
- `drone_*` naming as the public pool surface
- drone-specific state namespaces as the conceptual model

The Daystrom-inspired tier and capability mapping should underpin:

- profile-backed atoms
- workflow actors and atom bindings
- direct agents and adapter-owned scout dispatches
- raw bro exec when the caller opts into allocation

## 20. Open Design Questions

- Should output schemas always derive `structured_output`, or only when the
  dispatch surface can enforce provider-native schema mode?
  Recommendation: derive it when the output is machine-consumed as a contract;
  allow explicit best-effort parsing as an escape hatch.
- How should model-level capability metadata be authored and validated?
  Recommendation: design for lane-level tags now, even if v1 data starts
  provider-level.
- Which request-derivation decisions belong in system-default rule packets vs.
  plain allocator config?
- How much selection-policy expressiveness is enough before it becomes an unsafe
  custom algorithm language?
