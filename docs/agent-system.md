# Agent System

Agents are the old reusable-dispatch surface: a named persona, a brofile, a
prompt template, and search metadata. They still matter because existing
`bro_agent_*` callers and legacy workflow examples use them.

For new public capabilities, use [Atoms](atoms.md). An atom can still wrap a
brofile, but it also gives callers schemas, effect limits, child-call rules,
owned invocation handles, and non-LLM backends.

## Agent anatomy

An agent artifact carries:

| Field | Purpose |
|---|---|
| `name` | Unique agent name (canonical: `name@vN`) |
| `version` | Semver string |
| `description` | What this agent does |
| `when_to_use` | Heuristic for when to dispatch this agent |
| `anti_patterns` | When NOT to use this agent |
| `brofile` | Resolved persona (provider, model, effort, MCP filters) |
| `prompt_template` | Template with `{{args.field}}` expansion |
| `dispatch_adapter` | Optional custom dispatch path (e.g. `badgey`) |
| `cost_class` | `cheap` / `standard` / `expensive` |
| `allow_recursion` | Declares a fan-out orchestrator: dispatch keeps the recursive `bro_*` tools available and injects the orchestrator directive. Reviewed at install; there is deliberately no per-call override on `bro_agent_dispatch` (ad-hoc recursive dispatch is `bro_exec`'s `allow_recursion`) |
| `provenance_kind` | `bootstrap` / `learned` / `installed` |

## Installing agents

Agents are installed as artifacts. Blackbox-owned defaults live under
`system-defaults/agents/`; tutorial and integration examples stay under
`examples/`.

The source can be a local JSON file or an HTTP URL:

```json
// From a local file
bbox_artifact_install(
  kind = "agent",
  source = "/path/to/pr-reviewer.agent.json"
)

// From a URL
bbox_artifact_install(
  kind = "agent",
  source = "https://registry.example.com/agents/code-reviewer@v1.0.0.json"
)
```

## Discovering agents

```json
// List all active agents
bro_agent_list()

// Search by capability
bro_agent_search(query = "code review PR diff")
bro_agent_search(query = "security audit", cost_class = "cheap")

// Get full details for one agent
bro_agent_get(name = "pr-reviewer")

// Agent describe - full manifest + resolved brofile + merged filters
bro_agent_describe(agent = "pr-reviewer@v1.0.0")
```

For new capability discovery, prefer `atom_search` / `atom_describe`. Agent
manifests do not carry effect or composition policy, so they are weaker as a
public contract.

## Dispatching an agent

```json
bro_agent_dispatch(
  agent = "pr-reviewer",
  cwd = "/path/to/repo",
  args = {
    "pr_number": "142",
    "base_branch": "main",
    "focus": "security, correctness"
  }
)
```

Returns `{task_id, session_id, agent_label}`. The task is now a regular bro
task; use `bro_status`, `bro_wait`, and `bro_cancel` for lifecycle control.

Agents dispatch one bro task. If you need a reusable operation that can also be
bound into a workflow, expose the brofile through a profile-backed atom instead.

## Tool surface (filter merging)

Every dispatched agent has a computed tool surface determined by a deny-wins
merge of four layers:

1. **Global MCP config** (`~/.bro/mcp.json`) - allow/disallow patterns
   from `bro_mcp action=allow/disallow`
2. **Project overlay** (`<project>/.bro/mcp.json`)
3. **Brofile persona** - `allow_tools` / `disallow_tools` on the
   brofile the agent uses
4. **Per-dispatch overlay** - `allow_tools` / `disallow_tools` on the
   exec call itself

Layer 4 is the most specific and overrides everything. The mechanical recursion
guard (`bro_*` disallowed by default) applies at argv construction for every
dispatch-capable provider. Each provider gets its native deny syntax: Claude
`--disallowedTools`, Codex
`-c mcp_servers.blackbox.disabled_tools=[...]`, Gemini `--policy <tempfile>`.

## Lifecycle

Agents can be superseded:

```json
bbox_artifact_supersede(
  kind = "agent",
  name = "pr-reviewer@v1.0.0",
  superseded_by = "pr-reviewer@v2.0.0"
)
```

Superseded agents are excluded from `bro_agent_list` by default
(use `include_superseded=true` to see them).

## Custom dispatch adapters

Agents can declare a `dispatch_adapter` that routes through a
non-standard execution path. The built-in ones:

| Adapter | Purpose |
|---|---|
| *(none)* | Standard bro exec via brofile → argv → spawn |
| `badgey` | Routes through Badgey's consultant wrapper. The agent runs as a Badgey instance with its own thread-of-record and proposal store. |

Custom adapters are added by extending the provider catalog in
`src/orchestration/providers.rs`.

## When to install an agent vs. inline a prompt

| Situation | Tool |
|---|---|
| "I dispatch this same persona at least 3 times across different sessions" | Install as agent |
| "I want other agents to be able to discover and dispatch this" | Install as agent |
| "This is a one-off task" | Inline the prompt in `bro exec` |
| "This is a recurring pattern but the prompt changes each time" | Write a brofile with `bro_brofile` and use `bro exec --brofile` |

## Agent vs. Atom

| Situation | Prefer |
|---|---|
| Existing `bro_agent_*` caller or compatibility workflow | Agent |
| Persona-only dispatch with no stable input/output contract | Agent or brofile |
| Discoverable capability with schemas, effect limits, and trace handles | Atom |
| Capability implemented by a workflow, deterministic runner, or adapter | Atom |
| Workflow wants a reusable capability boundary | Atom plus workflow `atom_bindings` |

## See also

See also:

- [Rule Packets](rule-packets.md): agents use packets for deterministic classification
- [Workflow Engine](workflows.md): agents are dispatched as actor nodes; atoms are bound through `atom_bindings`
- [Atoms](atoms.md): use these for new reusable surfaces
- Design docs: `design/orchestration/agents/agent-system.md`, `design/orchestration/agents/agent-system-impl.md`
