---
title: "Brofile Context Templates"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - workflows
status: "proposed"
brief: "Designs template-based context assembly for brofiles, turns, dispatch defaults, and workflow actors."
---

# Brofile Context Templates

## Problem

Blackbox currently assembles prompt context in several ad hoc places:

- `apply_ambient` prepends scope, scoped pins, an unconditional recall
  directive, an unconditional task-shape hint, an orchestrator hint when
  `allow_recursion` is set, an optional completion contract, and an optional
  workspace-tools appendix when the brofile coerces workspace tools.
- `apply_brofile_lens` prepends brofile persona text.
- profile-backed atoms expand `inputs.prompt_template` and then dispatch via a
  brofile.
- workflow actor nodes render `NodeSpec.prompt` and then dispatch through
  `workflow_dispatch_executor` / `workflow_dispatch_ensemble`.
- provider CLIs may independently load global/user/project markdown such as
  `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `BLACKBOX.md`, `PROJECT.md`, provider
  config, and `@` includes.

The immediate failure mode is simple: lightweight executor bros can receive far
too much context, and callers do not have a clear way to say which first-turn
and resume-turn prompt wrappers a bro should use.

This design keeps the fix small: brofiles own prompt-rendering policy.

## Direction

A brofile may define context templates for first turns and resume turns. The
templates render with a bounded context object. Provider markdown/default-context
controls stay outside the template as provider launch policy.

There is no new context workflow runtime, context graph, hook registry, or
session context object in v1. When turn construction needs dynamic work, it
uses existing atom/workflow/rule-packet machinery as an explicit context
producer.

## Goals

1. Let brofiles define first-turn and resume-turn prompt wrappers.
2. Make lightweight bro exec/resume cheap and predictable.
3. Let callers suppress or replace provider-loaded markdown where providers
   support it.
4. Add dry-run inspection for the rendered prompt and provider context controls.
5. Let atoms/workflows populate template inputs without forcing that material
   into prompt strings.
6. Fix the current `bro_broadcast` resume inconsistency.

## Non-Goals

- Do not build a general context assembly graph.
- Do not introduce implicit or hidden dynamic discovery hooks in v1. Any lookup
  used for turn construction must be produced by an explicit atom/workflow
  context producer before rendering.
- Do not require workflow dispatch to use context producers. Workflow dispatch
  only gets heavier when an actor node or brofile explicitly names one.
- Do not store provider transcript content in blackbox for continuity. Provider
  sessions already own conversation continuity.
- Do not replace MCP tool filtering or `bbox_mcp_surface`; tool policy remains a
  separate dispatch concern.

## Brofile Schema

Add an optional `context` block to brofiles:

```json
{
  "name": "drone-probe-codex",
  "provider": "codex",
  "model": "gpt-5.5",
  "context": {
    "provider_defaults": "suppress_when_supported",
    "harness_markdown": {
      "policy": "deny_list",
      "deny": ["AGENTS.md", "BLACKBOX.md", "PROJECT.md"],
      "follow_includes": false
    },
    "first_turn": {
      "template_file": "system-defaults/prompts/bro/drone-first.tera",
      "context_producer": "atom:context/atom-signposts@v1"
    },
    "resume_turn": {
      "template_file": "system-defaults/prompts/bro/drone-resume.tera",
      "context_producer": "atom:context/resume-delta-light@v1"
    }
  }
}
```

Template source can be one of:

- `template`: inline Tera template text.
- `template_file`: local Tera file reference. This is the preferred reusable
  form. Files may live in system defaults, project `.bbox/prompts`, or
  operator-owned prompt directories. Use `.tera` for templates.
- `template_ref`: optional artifact/catalog alias that resolves to a prompt
  file.

## Template Resolution

`template_file` resolution is explicit and trust-scoped. Three roots are
recognized:

- `builtin`: shipped templates under `system-defaults/prompts/`. Not
  configurable; tied to the running daemon binary.
- `project`: `<project_dir>/.bbox/prompts/`. Resolved from the dispatch's
  project scope; no env override.
- `user`: `$XDG_CONFIG_HOME/blackbox/prompts/` by default, overridable via
  `BLACKBOX_USER_PROMPTS_DIR` (allowlist-style env var, listed in
  `config.rs`).

Resolution rules:

- Absolute paths are allowed only when they lie under one of the three roots
  above.
- Relative paths resolve first under the project root, then under builtin
  system defaults. A `project:` / `builtin:` / `user:` prefix can force a root.
- Paths are unsafe if they escape the selected root, traverse through `..`,
  resolve through a symlink outside the selected root, or are not UTF-8.

`template_ref` resolves through the installed artifact catalog during brofile
validation or dry-run. v1 adds a new `Prompt` variant to `ArtifactKind` in
`src/artifacts.rs` so prompt templates participate in the same install / list /
supersede / remove lifecycle as workflows, packets, brofiles, agents, atoms,
teams, and crons. Missing refs fail closed. Runtime dispatch may use a cached
resolved template body/hash from validation, but dry-run must still show the
ref, source path, trust scope, and content hash.

## Turn Templates

First-turn and resume-turn templates are separate on purpose.

First turn often needs role/persona anchoring:

```tera
{% if scope %}[scope]
{{ scope }}
{% endif %}
{% if lens %}{{ lens }}
{% endif %}
{{ prompt }}
```

Resume turn is usually much smaller because the provider transcript already has
the prior persona and task history:

```tera
{% if scope %}[scope]
{{ scope }}
{% endif %}
{{ prompt }}
```

The template language is the "AST-shaped lambda": it provides conditionals,
loops, includes, macros, and composition without blackbox inventing another
workflow runtime.

Templates do not call tools directly in v1. The existing `src/template.rs`
helper renders Tera from a JSON context and does not register custom functions.
Dynamic inputs are produced before Tera rendering by the turn's configured
context producer. Do not hide tool calls inside template rendering.

## Template Context

Render context should be explicit and bounded:

```rust
struct PromptRenderContext {
    turn: TurnKind,              // first | resume
    prompt: String,              // current user/operator prompt
    scope: Option<String>,       // task/session/project scope block
    task_id: String,
    session_id: Option<String>,
    project_dir: Option<String>,
    brofile_name: Option<String>,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    lens: Option<String>,
    pins: Option<String>,
    recall_directive: Option<String>,
    task_shape_hint: Option<String>,
    orchestrator_hint: Option<String>,
    completion_contract: Option<String>,
    workspace_tools_appendix: Option<String>,
    atom: Option<AtomRenderContext>,
    workflow: Option<WorkflowRenderContext>,
    agent: Option<AgentRenderContext>,
    template_inputs: serde_json::Value,
}
```

The base render context is built by the bro dispatch assembler from existing
inputs: current prompt, brofile, provider/model/effort, task/session IDs,
project scope, pins, and the same ingredients `apply_ambient` currently uses.
Fields are optional so minimal templates can ignore them, but they are ordinary
bounded strings, not hidden search results.

Large historical memories, runbooks, thread dumps, and search results do not
belong in the base context. If a bro needs those, select a brofile whose
context producer returns them as bounded `template_inputs`.

Dispatch-specific context objects are bounded metadata, not full upstream
state dumps. The JSON shape Tera sees is the public contract — locked in v1
so prompt templates do not break when the underlying runtimes evolve:

```rust
struct AtomRenderContext {
    atom_ref: String,                 // "atom:context/atom-signposts@v1"
    invocation_id: Option<String>,
    implementation_kind: String,      // "deterministic" | "adapter" | "workflow_backed" | "profile"
    input_schema_name: Option<String>,
    output_schema_name: Option<String>,
}

struct WorkflowRenderContext {
    workflow_name: String,
    workflow_version: String,
    arc_id: String,
    node_id: String,
    actor_name: String,
    imports: Vec<String>,             // declared import names only
    exports: Vec<String>,             // declared export names only
}

struct AgentRenderContext {
    agent_ref: String,                // "agent:reviewer/code-review@2"
    agent_label: Option<String>,
    manifest_version: String,
}
```

`WorkflowRenderContext.imports` / `exports` are name lists, not values; full
`ArcContext.vars` is never serialized into the render context. `AtomRenderContext`
exposes schema *names*, not schema bodies. Changing or extending these structs
in later versions requires versioning the field (e.g. adding fields is
forward-compatible; removing or repurposing one is a template-break and must
be coordinated through brofile schema versioning).

If one of these fields is absent, templates see `null`. If richer upstream
state is needed, it must be summarized by a context producer into
`template_inputs`.

## Context Producers

Pre-turn lookup work belongs to an existing orchestration artifact, not to the
brofile template. A turn may name a `context_producer`, which is an atom or
workflow reference that returns bounded template input data before the brofile
template renders.

`context_producer` is turn-local. `first_turn.context_producer` and
`resume_turn.context_producer` may point at different producers, and either may
be absent. The normal shape is a heavier first-turn producer and no resume
producer, or a much smaller resume producer that fetches only fresh deltas. If
both turns reference the same producer, the producer still receives `turn` and
can branch internally.

The producer input is a JSON object:

```json
{
  "turn": "first",
  "prompt": "<current operator prompt>",
  "task_id": "task-...",
  "session_id": null,
  "project_dir": "/repo",
  "brofile_name": "drone-probe-codex",
  "provider": "codex",
  "model": "gpt-5.5"
}
```

The producer output is:

```json
{
  "template_inputs": {
    "atom_signposts": []
  },
  "warnings": [],
  "receipts": []
}
```

Dispatch attaches producer `template_inputs` to the render context. The
producer owns its output schema, warnings, and receipts.

Output caps are enforced by dispatch, not by the producer, so a misbehaving
producer cannot blow out the rendered prompt. v1 defaults:

- 32 KB total across `template_inputs`.
- 8 KB per top-level key.

Both caps are overridable per turn via optional `context.first_turn.caps` /
`context.resume_turn.caps` fields. When a cap is exceeded, dispatch truncates
the offending value, drops it from `template_inputs`, and adds a structured
warning to the receipt; the template renders without that key.

Producer failure policy is render-without, never fail-closed by default.
If the producer errors, times out, or returns malformed output, dispatch:

1. logs a `dispatch.context_producer.failure` system event with the producer
   ref, error, and elapsed time;
2. emits a warning into the turn receipt;
3. renders the template with empty `template_inputs`.

Templates are responsible for handling absent inputs (the `{% if %}` guards in
the examples above show the pattern). A future opt-in `on_failure: fail` knob
can be added per turn, but the default keeps long-lived bros from getting
stranded by a flaky producer.

Producer results are not cached in v1. Every first turn invokes the
first-turn producer; every resume turn invokes the resume-turn producer. v1
producers are cheap by construction (`atom_search` / `atom_describe` /
`bbox_knowledge` over local stores); when measured cost justifies it, a
`(producer_ref@version, input_hash, brofile_hash)` task-scoped cache can be
added without changing the contract.

Context producers reuse the existing machinery:

- Workflow-backed producers use `ArcContext`, `on_enter` hooks, `mcp_call`,
  `atom_bindings`, subworkflows, and rule-packet gates.
- Atom-backed producers provide a compact reusable contract for common
  population tasks, and may themselves be workflow-backed.
- Rule packets remain deterministic classifiers/gates inside the workflow or
  atom. They select, validate, or stop context population; they do not fetch
  data by themselves.

Bro dispatch invokes a producer through the atom/workflow runtime and consumes
only its declared output contract. It does not interpret workflow nodes, packet
AST, hook ops, or MCP call plans.

Invocation is by ref kind:

- `atom:*`: call the internal atom invocation path with the producer input
  JSON. The atom must declare a `context_producer` capability and an output
  schema containing `template_inputs`, `warnings`, and `receipts`.
- `workflow:*`: run the installed workflow synchronously with the producer input
  as initial `ArcContext.vars`; the workflow must export the same producer
  output shape.

V1 context producers are non-agentic: deterministic atoms, adapter atoms, or
workflow-backed producers that do not dispatch provider actors. They may call
read-only tools such as `atom_search`, `atom_describe`, and `bbox_knowledge`
through existing adapter/hook machinery, and may use rule packets to select,
validate, or stop. A future phase can explicitly reopen provider-dispatching
producers, but ordinary `bro_exec` / `bro_resume` must not silently spawn
another agent before dispatching the requested bro.

This is enforced mechanically, not by convention. Atoms gain a
`capabilities: ["context_producer"]` flag in their manifest; the atom registry
refuses to install or register an atom that combines this capability with a
profile-backed implementation (the implementation kind that dispatches a
provider actor). Workflow-backed producers go through the same capability
check at registration time. Producer resolution at dispatch time then only
verifies the capability is present and skips the agentic-shape check entirely,
keeping the hot path simple.

For atom signposting, the reusable producer can be an atom such as
`atom:context/atom-signposts@v1`. Internally, that atom can call `atom_search`,
optionally call `atom_describe`, apply a packet to cap/filter results, and
return `template_inputs.atom_signposts`.

The matching template can decide how much of that material to expose:

```tera
{% if template_inputs.atom_signposts %}
[atom signposts]
{% for atom in template_inputs.atom_signposts.results %}
- {{ atom.name }}: {{ atom.when_to_use }}
{% endfor %}
{% endif %}
{{ prompt }}
```

This keeps bro dispatch from becoming a second workflow runtime. If the needed
population logic has branching, mutation, retries, or complex tool
orchestration, put that logic in a workflow-backed context producer and pass
only its summarized output as `template_inputs`.

Determinism is receipt-based. Search results may change as blackbox state
changes, so dispatch records the producer ref/version, input hash, result IDs
where available, byte count, output hash, status, and timestamp. Dry-run returns
the same receipt shape without launching a provider session. Resume turns use
the brofile's current `resume_turn.context_producer`; the caller does not need
to remember the first-turn context producer.

## Provider Defaults

Provider-loaded markdown is separate from daemon-rendered prompt text:

```json
{
  "context": {
    "provider_defaults": "suppress_when_supported",
    "harness_markdown": {
      "policy": "deny_list",
      "deny": ["AGENTS.md", "BLACKBOX.md", "PROJECT.md"],
      "follow_includes": false
    }
  }
}
```

Modes:

- `default`: let the provider load its normal global/user/project markdown.
- `suppress_when_supported`: suppress provider markdown when the provider has
  known controls; warn when unsupported.
- `strict_suppress`: fail if suppression is requested but unsupported.
- `explicit_only`: use only the rendered prompt or generated instruction file.

`harness_markdown` is the file-level lever for harness/provider markdown such
as `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `BLACKBOX.md`, `PROJECT.md`, and
their `@` includes. It is intentionally separate from template rendering.

For OpenCode/Inception, `harness_markdown` uniformly governs the generated
opencode `instructions` array — including the current `BLACKBOX.md` entry and
any future `AGENTS.md`-style entries. `build_opencode_config` filters the
candidate list through the policy before writing the config; there is no
secondary mechanism for opencode-specific exemptions.

Fields:

- `policy`: `default`, `suppress_all`, `allow_list`, or `deny_list`.
- `allow`: file basenames, absolute paths, or project-relative globs allowed
  when `policy=allow_list`.
- `deny`: file basenames, absolute paths, or project-relative globs suppressed
  when `policy=deny_list`.
- `follow_includes`: whether provider/harness `@` includes are allowed to pull
  additional files after the root file is allowed.

If a provider cannot enforce the requested file-level markdown policy, dispatch
warns under `suppress_when_supported` and fails under `strict_suppress` or
`explicit_only`. Dry-run must report discovered, injected, suppressed, and
unsupported markdown files where the provider exposes enough information.

Provider mapping must be explicit in dry-run. The table below records current
blackbox launch paths and v1 targets. Any concrete suppression flag must be
verified against the installed provider CLI before implementation; until then,
the provider reports unsupported suppression rather than pretending to be clean.

| Provider | Current exec/resume prompt path | Current default-context behavior | V1 provider-default control |
|---|---|---|---|
| Claude | `claude -p <prompt>` and `claude --resume <session> -p <prompt>` with transient `--mcp-config` | CLI loads its normal Claude settings/instruction files; blackbox does not currently suppress them | Initially report suppression unsupported unless verified CLI flags/config are implemented. If implemented, dry-run must show exact args/config. |
| GLM | Claude-compatible CLI path using `claude -p` / `--resume` with GLM model/env | Same as Claude-compatible transport | Same as Claude-compatible path; no silent claim of suppression. |
| DeepSeek | Claude-compatible CLI path using `claude -p` / `--resume` with DeepSeek model/env | Same as Claude-compatible transport | Same as Claude-compatible path; no silent claim of suppression. |
| Codex | `codex exec <prompt>` and `codex exec resume <session> <prompt>` | CLI may load Codex rules/user config; blackbox currently passes only prompt/model/effort/cwd | Add and dry-run verified Codex controls such as rule/user-config suppression flags or generated instruction-file controls where supported. |
| Copilot | `gh copilot -- -p <prompt>` and `--resume=<session>` | Provider behavior is inherited from Copilot CLI; no suppression control currently wired | Report suppression unsupported until a concrete CLI/config mechanism is verified. |
| Gemini | `gemini -p <prompt>` and `gemini --resume <session> -p <prompt>` | Gemini CLI default context behavior is not controlled by blackbox today | Report suppression unsupported until exact Gemini controls are verified. MCP tool exclusions are separate tool policy, not markdown suppression. |
| Vibe | `vibe -p <prompt>` and `vibe --resume <session> -p <prompt>` | Vibe config/default context behavior is not controlled by blackbox today | Report suppression unsupported until exact Vibe controls are verified. |
| Inception/OpenCode | `opencode run <prompt>` / `opencode run --session <session> <prompt>` | blackbox writes a generated `OPENCODE_CONFIG` file and, when `BLACKBOX.md` exists, lists it in the opencode `instructions` array so opencode merges it into the system prompt | Move the `instructions` write behind `provider_defaults`; dry-run shows whether `BLACKBOX.md` is included, suppressed, or replaced in the `instructions` array. |

`suppress_when_supported` is therefore best-effort and warning-heavy. It must
not pretend unsupported providers are clean. `strict_suppress` fails closed on
any provider without a verified mechanism.

The v1 deliverable for Phase 2 is the schema plus the report-and-warn
behavior. Codex and OpenCode/Inception ship as enforcing providers (the
former via verified CLI/config controls, the latter via the `instructions`
array). Every other provider accepts the policy field, but dry-run reports
it as `unsupported_suppression` and dispatch logs a warning at launch.
Wiring additional providers as enforcing is a follow-up patch per provider,
not a phase gate.

## Existing Layers As Templates

Current ambient sections become template variables or built-in partials:

- `scope`
- `pins`
- `recall_directive`
- `task_shape_hint`
- `orchestrator_hint` (currently emitted only when `allow_recursion` is set)
- `completion_contract`
- `workspace_tools_appendix`
- `lens`

Profiles such as `drone-minimal` do not need a separate policy engine. They can
be shipped as brofiles or prompt template refs that simply do not include those
partials.

Example lightweight drone:

```json
{
  "name": "drone-minimal",
  "provider": "deepseek",
  "model": "deepseek-v4-pro",
  "lens": "You are a fast executor. Follow the prompt exactly and report concise results.",
  "context": {
    "provider_defaults": "suppress_when_supported",
    "first_turn": {
      "template": "{% if scope %}[scope]\n{{ scope }}\n{% endif %}{% if lens %}{{ lens }}\n{% endif %}{{ prompt }}"
    },
    "resume_turn": {
      "template": "{{ prompt }}"
    }
  }
}
```

## Dispatch Behavior

`bro_exec`:

1. Resolve brofile.
2. Build base `PromptRenderContext` for `turn=first`.
3. If `context.first_turn.context_producer` is set, invoke that atom/workflow
   producer and merge its capped `template_inputs`.
4. Render `context.first_turn` if present; otherwise use current legacy
   `apply_ambient` + `apply_brofile_lens` behavior.
5. Apply provider-default policy.
6. Launch provider.

`bro_resume`:

1. Use the requested provider/session ID or resolve the named bro's latest
   provider/session ID.
2. Resolve the current brofile when a named bro is used.
3. Build base `PromptRenderContext` for `turn=resume`.
4. If `context.resume_turn.context_producer` is set, invoke that atom/workflow
   producer and merge its capped `template_inputs`.
5. Render `context.resume_turn` if present; otherwise use current legacy
   resume behavior.
6. Resume the provider session.

Provider conversation continuity requires only provider + session ID + rendered
new prompt. The prior transcript remains provider-owned.

If the brofile changes between turns, default behavior intentionally uses the
current brofile's resume template and context producer. That keeps control with
the current brofile policy instead of persisting a blackbox-owned session
context object. Pinning old template behavior can be added later if operators
actually need it, but it is not part of v1.

To make brofile drift visible without inventing a pinning mechanism, every
dispatch emits a `dispatch.template_resolved` system event with the brofile
name, brofile version, resolved template ref (or inline-hash for literal
templates), template content hash, and producer ref/version. A long-lived bro
whose brofile is edited mid-arc therefore shows a clear event boundary on the
first turn that picks up the new policy.

## Atoms And Workflows

Profile-backed atoms already render atom input into a prompt and dispatch via a
brofile. Under this design, the atom prompt becomes the `prompt` input to the
brofile's first-turn template.

Atom `inputs.prompt_template` keeps its existing simple-placeholder grammar:
`{{name}}` references resolve against the atom's declared input schema and are
validated by `validate_prompt_template` in `src/orchestration/atoms/validate.rs`.
That grammar is intentionally distinct from the Tera grammar used by brofile
turn templates — atom inputs render to a `prompt` string, and the brofile
template then composes that string with scope, pins, lens, and template inputs.

Workflow actor nodes already render `NodeSpec.prompt` from `ArcContext`. That
rendered node prompt becomes the `prompt` input to the actor brofile's first or
resume template. Workflow does not need a special context assembly system for
v1; if workflow-specific context should influence rendering, the bounded
`workflow` render context and/or a workflow-backed `context_producer` carries
that contract.

Deterministic and adapter atoms do not use provider prompt templates.

## Broadcast Resume Fix

Current `bro_broadcast` fresh member dispatch applies ambient + lens, but
resumed members receive the raw prompt. That differs from normal `bro_resume`,
which reapplies ambient.

With brofile templates:

- fresh broadcast member uses the member brofile's first-turn template.
- resumed broadcast member uses the member brofile's resume-turn template.

This makes broadcast consistent with ordinary exec/resume while still allowing a
minimal resume template such as `{{ prompt }}`.

## Dry Run

Add:

```text
bro_context(action="dry_run", ...)
```

Dry-run returns:

- brofile name
- provider/model/effort
- turn kind
- selected template source (`template`, `template_file`, or `template_ref`)
- selected context producer, if any, with producer ref/version, input hash,
  output keys, result IDs where available, byte counts, warnings, and output
  hash
- rendered prompt size/hash and optional content preview
- provider-default mode
- provider argv relevant to markdown/default-context controls, with secrets
  redacted
- harness markdown policy plus discovered, injected, suppressed, and
  unsupported file entries where available
- MCP filter/surface summary as separate tool policy information
- warnings for unsupported suppression, missing template refs, or unsafe
  template paths

Dry-run must not create provider sessions or mutate task stores.

## Template Registry

Prompt templates can be stored as normal installed artifacts or system-default
files. The registry is the existing artifact catalog plus builtin files; it
needs only:

- name/ref
- version
- template body or file source
- trust scope (`builtin`, `project`, `user`)
- optional description

Trust scope is enforced during template resolution: builtin refs resolve only
to shipped templates, project refs resolve only within the current project, and
user refs resolve only within configured user prompt roots. Do not make prompt
templates a new agent/atom execution surface. They are text renderers.

v1 adds a `Prompt` variant to the `ArtifactKind` enum in `src/artifacts.rs`
(currently `Workflow`, `Packet`, `Brofile`, `Agent`, `Atom`, `Team`, `Cron`).
Prompt templates participate in the same install / list / supersede / remove
lifecycle as other artifacts so `template_ref` has a real backing catalog
entry; resolution does not depend on filesystem layout alone.

## Migration Plan

### Phase 1: Template Renderer

- Add brofile `context` schema (the `Brofile` struct in
  `src/orchestration/brofile.rs` has no `context` field today).
- Add a `Prompt` variant to `ArtifactKind` in `src/artifacts.rs` with the
  standard install / list / supersede / remove plumbing.
- Add Tera rendering for `first_turn` and `resume_turn` using the existing
  `src/template.rs` helper.
- Add plain `.tera` prompt file loading with trust-scoped path/ref resolution
  using the builtin / project / user roots defined above.
- Add `context_producer` references on turn templates. Producers are atoms or
  workflows, not inline brofile lookup declarations.
- Add the non-agentic context-producer invocation contract, output schema,
  and the `capabilities: ["context_producer"]` registry check.
- Ship built-in legacy templates (`builtin:prompts/legacy-full@v1` and
  `builtin:prompts/legacy-resume@v1`) reproducing the current
  `apply_ambient` + `apply_brofile_lens` output. When a brofile has no
  `context` block, the renderer resolves to those refs — there is one render
  path, not two. The Phase 4 cleanup then has nothing to remove beyond the
  Rust helpers themselves.
- Ship a minimal drone template ref for lightweight executor brofiles.
- Lock the `PromptRenderContext` / `AtomRenderContext` / `WorkflowRenderContext`
  / `AgentRenderContext` JSON shapes as serde structs; field additions are
  forward-compatible, removals require brofile schema version bumps.
- Add `bro_context(action="dry_run")`.

### Phase 2: Provider Defaults

- Add provider-default suppression modes.
- Add harness markdown file-level policy parsing and dry-run reporting.
- Start by verifying and wiring the installed Codex CLI controls.
- Move the OpenCode generated-config `instructions` array (currently always
  including `BLACKBOX.md` when it exists, via `build_opencode_config`) behind
  provider-default policy so suppression and replacement are explicit.
- Report unsupported suppression in dry-run and dispatch warnings.

### Phase 3: Dispatch Integration

- Route `bro_exec`, `bro_resume`, and `bro_broadcast` through brofile templates.
- Route profile-backed atom dispatch through the target brofile template.
- Route workflow executor/ensemble dispatch through actor brofile templates.
- Fix broadcast resume inconsistency as part of this integration.
- Add a regression test asserting that recursion-guard filters from
  `resolve_dispatch_filters` appear in argv for both fresh and resumed
  broadcast members. The textual ambient layer moving into a template must
  not silently lift the mechanical guard.
- Emit a `dispatch.template_resolved` system event per turn carrying brofile
  name/version, template ref/hash, and producer ref/version, so brofile
  drift mid-arc is visible in the event log.

### Phase 4: Cleanup

- Convert current `apply_ambient` / `apply_brofile_lens` behavior into built-in
  templates or template partials.
- Keep `apply_ambient` compatibility wrappers only as transitional helpers.
- Add regression tests for legacy full rendering, minimal drone rendering,
  resume template rendering, context producer invocation,
  provider-default suppression warnings, and broadcast fresh/resume consistency.

## Consensus Defaults

- Lightweight executor brofiles should use small first-turn templates and
  smaller resume-turn templates.
- Resume does not need a blackbox-owned "session context" object. The provider
  session is the conversation continuity.
- Tool policy remains outside prompt templates.
- Dynamic discovery belongs in explicit context producers or the
  caller/orchestrator before bro dispatch. Prompt templates receive only
  explicit rendered prompt text and template inputs.
