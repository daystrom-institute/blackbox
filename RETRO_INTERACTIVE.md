# Interactive Retro

A retrospective pass for an **outbound interactive agent session**: Codex,
Claude, or another live agent working directly with an operator through the
interactive tool surface. Use it to preserve friction and useful surprises from
the live agent's point of view, especially when that experience differs from a
dispatched bro or `bro-harness` session.

This is a manual, human-steered pass. Point the interactive agent at this
document near the end of a meaningful session, especially when the session
involved tool discovery, MCP setup, repo instructions, worktree/git closeout,
or operator steering. The purpose is close to `bro_retro`: invite concrete
substrate feedback without compelling the agent to invent any. Unlike
`RETRO_HARNESS.md`, this does not focus on the `bro-harness` promise model or
intern. It focuses on what the interactive agent actually had: its native tool
catalog, MCP availability, shell environment, context/instruction stack, and
conversation with the operator.

## What this reflects on

The subject is the live interactive operating experience, not the product work
itself.

- **Interactive tool surface** - the tools available to the agent in the live
  session: shell execution, patching, web lookup, image/view tools, MCP
  discovery, newly loaded MCP namespaces, and any provider-specific tools. Was a
  needed tool missing, hidden until discovery, awkwardly shaped, or only
  available after a new session?
- **MCP setup and routing** - how MCP servers were discovered, configured, and
  validated from the interactive session. Did config live somewhere surprising?
  Did a server name, transport, binary path, socket, surface, or restart
  requirement cause friction? Did requests land on the intended project or
  default socket?
- **Instruction and context stack** - system/developer instructions, repo
  `AGENTS.md`, project docs, work packets, summaries after compaction, and
  operator messages. Were instructions clear, stale, contradictory, too large,
  missing a key invariant, or hard to map onto the task?
- **Operator interaction** - how the operator steered the work: interruptions,
  corrections, scope changes, approvals, closeout requests, and "do this like
  that previous thing" references. What made collaboration easier? What was
  confusing or caused a false start?
- **Local environment** - OS, shell, path layout, config directories, wrappers
  such as `rtk`, Cargo target/cache behavior, tmux sockets, macOS vs Linux path
  differences, and host-local state. What surprised the agent? What would
  future-you need documented to avoid the same trap?
- **Interactive persistence and session boundaries** - what did and did not
  survive config changes, compaction, new tool discovery, or session restart.
  Did the agent incorrectly assume that a newly configured tool was immediately
  available? Did a compacted summary preserve enough state?
- **Git and multi-agent locality** - worktree creation/removal, bystander dirty
  files, scoped staging, closeout, and shared-service caution. Did the
  instructions and environment make it clear what was safe to mutate?

## The Prompt

Run this as a non-compelling retrospective. It asks for reflection on the
interactive operating experience, not more product work.

> Quick optional reflection - the task itself is done, nothing more is needed
> on it.
>
> While it is still fresh, thinking only about the live interactive experience
> you just had, did anything about the tools, MCP setup, repo instructions,
> operator interaction, or local environment get in your way? Worth a mention if
> it comes to mind:
>
> - a tool or MCP namespace you reached for that was missing, hidden, stale, or
>   only became available after config/session changes;
> - an MCP setup or routing step that was confusing: server name, command path,
>   transport, socket, project target, config file, or validation path;
> - system, developer, repo, or work-packet instructions that were unclear,
>   stale, contradictory, overbroad, or missing an important local invariant;
> - something about the operator interaction that caused confusion, delay, or a
>   false assumption, including interruptions, scope shifts, or implicit context;
> - an OS, shell, wrapper, path, config-directory, cache, tmux, or worktree
>   detail that surprised you and would help future-you if documented;
> - something that worked well and should be considered as durable project or
>   environment knowledge, without you promoting it yourself.
>
> Scope matters: this channel is for things the operator, Blackbox substrate, or
> system instructions can plausibly improve. It is not for ordinary defects in
> the target repo, compiler errors, flaky third-party services, or generic
> product TODOs unless the friction was caused by how the interactive agent was
> instructed, tooled, or routed.
>
> If a concrete Blackbox substrate gap stands out - something a future agent or
> operator would genuinely be glad Blackbox knew - dedupe first with
> `bbox_gaps`, then file one gap per distinct issue with `bbox_gap` when that
> tool is available. Use the existing gap kinds; do not coin a new kind:
> `mcp_surface`, `tooling`, `workflow`, `agent`, `docs_runbook`,
> `refactor_primitive`, `ontology`, `eval_coverage`, `packet_ast`.
>
> If something is more like durable local/project knowledge than a gap, report
> it as a candidate for operator review instead of calling `bbox_learn` or
> changing memory yourself. The operator can decide whether it belongs in
> project knowledge, global instructions, repo docs, or nowhere.
>
> If nothing in-scope stands out, that is a completely normal way for a session
> to end. Say so in one line and file nothing. No quota, no expectation; a quiet
> run is a good run. Do not manufacture friction just to have something to say.

## Novel Interactive Axes

These are the areas most likely to differ from dispatched or harness agents:

- **Tool discovery latency** - interactive agents may need to search deferred
  tool metadata before a newly configured MCP namespace appears. A config change
  can be correct even when the current turn still lacks the tool.
- **Native vs MCP duplicate tools** - shell, tmux, git, and search may exist as
  both native tools and MCP tools. Note when the agent used a fallback and
  whether that fallback changed safety, ergonomics, or confidence.
- **Host-global config** - Codex, Claude, Blackbox, and repo-local configs can
  live in different places and use different schemas. Note any path that was
  non-obvious, stale, or platform-specific.
- **Operator authority in the loop** - the operator can interrupt, correct, or
  expand scope mid-turn. Reflect on whether those interventions were easy to
  incorporate or whether the agent kept carrying an older assumption.
- **Instruction density** - interactive sessions often combine global
  instructions, repo instructions, user-provided `AGENTS.md`, summaries, and
  live corrections. Identify what was load-bearing and what created ambiguity.
- **Environment asymmetry** - an interactive agent sees the real host: macOS
  paths, shell wrappers, attached tmux sessions, untracked bystander files,
  shared service constraints, and local build caches. These details may be
  invisible to a dispatched agent.
- **Validation expectations** - live agents may be asked to prove that tools
  work, not just configure them. Note whether the validation path was obvious
  and whether it risked mutating shared state.

## What Counts As A Gap

File or propose a gap only when the missing or wrong-shaped capability is
reusable and in scope for Blackbox or the interactive substrate.

Good examples:

- MCP tool discovery did not surface a configured server until the agent knew to
  call a discovery tool or start a new session.
- A server config had to be translated between Claude `.mcp.json` and Codex
  TOML, but there was no runbook for the schema/path difference.
- An MCP tool existed but omitted the polling/result tool needed to complete the
  normal workflow, forcing pane capture as a workaround.
- Instructions said to use a specific tool family, but the current session did
  not have that namespace exposed and gave no clear fallback rule.
- A host-local wrapper or shell behavior made normal checks misleading.
- The operator had to restate a local invariant that could have been known from
  repo docs or environment docs.

Do not file a gap for:

- Ordinary code bugs or product follow-ups in the target repo.
- One-off preferences that would not help another agent.
- External service behavior Blackbox cannot change.
- Durable facts that belong in project knowledge rather than the gap queue.

## Operator And Environment Retro

In addition to substrate gaps, explicitly ask what the operator or environment
could do better next time. This section is intentionally not a direct memory
write. It produces candidates the operator can review.

Ask:

- What did the operator say that clarified the task or prevented a bad path?
- What did the operator assume the agent already knew, but the agent had to
  infer or rediscover?
- Which system, developer, or repo instruction would you change, narrow, move,
  or delete?
- Was the work packet missing a concrete path, branch, config file, command, or
  acceptance criterion?
- Did interruptions or corrections arrive in a form that was easy to apply?
- What local environment surprise should future agents know before starting?
- What worked well enough that it may deserve durable documentation?

Output these as "operator/environment candidates", not as automatic
`bbox_learn` calls.

## Process

1. Walk the session chronologically and list moments of friction, surprise, or
   unusually smooth operation.
2. Separate product-work findings from interactive-operation findings.
3. Group reusable issues by capability or instruction/environment invariant.
4. For each gap candidate, ask whether another interactive agent on another
   session or project would plausibly hit it.
5. Dedupe real substrate gaps with `bbox_gaps` before filing `bbox_gap`, when
   those tools are available.
6. List operator/environment candidates separately for human review.
7. Note candidates deliberately not filed, with the reason.

## Gap-Filing Call

When `bbox_gap` is available and a real reusable substrate gap exists:

```text
bbox_gap(
  title="Short human-readable gap title",
  gap_kind="mcp_surface",
  domain="interactive/mcp-discovery",
  wanted_capability="Describe the reusable capability the interactive agent wanted.",
  dedupe_key="mcp_surface/interactive/mcp-discovery/capability-slug",
  impact="medium",
  blocking_level="workaround_available",
  missing_primitive="Optional concrete tool, config, prompt, or surface name.",
  fallback_used="What the agent did manually instead.",
  evidence=["interactive retrospective", "file:RETRO_INTERACTIVE.md"],
)
```

Required: `title`, `gap_kind`, `domain`, `wanted_capability`, `dedupe_key`.
Use existing gap kinds only. For interactive retros, common kinds are
`mcp_surface` (missing/awkward MCP tools or visibility), `tooling` (native tool
or config capability), `workflow` (closeout, validation, routing, interruption),
`agent` (interactive loop/tool-discovery behavior), and `docs_runbook`
(instructions, config docs, environment runbooks).

## Retrospective Output Shape

Return a short summary:

- Gaps filed: gap ids plus titles.
- Existing gaps reused or referenced: gap ids plus dedupe keys.
- Operator/environment candidates: one line each, with suggested destination
  if obvious (repo doc, project knowledge, global instruction, local runbook).
- Interactive tool assessment: what was missing, hidden, awkward, or effective.
- MCP/config assessment: whether routing, server setup, and validation were
  clear.
- Candidates not filed: one line each, with the reason.
- Follow-up risk: anything likely to keep hurting interactive agents if left
  untriaged.

## Tone

Be concrete and operational. Prefer "I reached for X, the interactive session
gave me Y, I worked around it with Z" over broad complaints. The goal is not to
criticize the operator or the task; it is to preserve reusable feedback from
the live agent's point of view before it dissolves into chat history.
