---
title: "Claude - Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: context-management
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - context-management
brief: "Claude Code 2.1.160 context construction separates cache-stable defaults, ambient project/user overlays, volatile per-machine sections, slash-command prompt expansion, event-triggered per-turn injections, and recap/away-summary status messages. UserPromptExpansion runs when a slash command/MCP prompt expands into prompt messages and can attach additionalContext or block expansion before the expanded prompt enters the turn."
---

# Claude - Context Management

> Evidence: direct observation of the running 2.1.160 harness, current claude --help, focused binary strings over /Users/invidious/.local/share/claude/versions/2.1.160, and prior bro-harness compaction/api-robustness mines. See [snapshot](claude-2.1.160.md).

See the axis: [Context Management](../context-management.md).

## Context Layers

Claude's context assembly is layered rather than a single every-turn prompt blob:

- Stable default system instructions and tool descriptions.
- Ambient overlays from project/user markdown such as CLAUDE.md and PROJECT.md, including @path imports.
- Dynamic per-machine/session sections: cwd, env info, memory paths, git status, model/session metadata, and similar host-specific state.
- Names-only deferred manifests for skills and MCP tools, with bodies/schemas loaded later through Skill or ToolSearch.
- Slash-command and MCP-prompt expansion, where a user command becomes generated prompt messages plus optional hook-added context.
- Event-triggered additions from hooks, reminders, permissions, compaction, subagents, and todo/task state.

The important design point is that volatile context is not necessarily placed in the cacheable system prefix. Current help exposes --exclude-dynamic-system-prompt-sections, which moves cwd, env info, memory paths, and git status from the system prompt into the first user message. The stated reason is cross-user prompt-cache reuse. The flag only applies with the default system prompt and is ignored with --system-prompt.

## Startup / First Turn

At normal startup the harness discovers CLAUDE.md/PROJECT.md overlays, MCP config, skills/plugins, agents, LSP/IDE state, settings, and memory. The first-turn environment block includes cwd, git status snapshot, platform/OS/date, and model identity. The git status snapshot is explicitly a point-in-time value, not a live-updating fact.

SessionStart hooks can change the first-turn shape. Binary hook schemas include SessionStart additionalContext, initialUserMessage, sessionTitle, watchPaths, and reloadSkills. That means a hook can both add model context and prepend an initial user message before the user's prompt. Changelog confirms reloadSkills lets SessionStart-installed skills become available in the same session.

Bare mode is the opposite startup profile. --bare sets CLAUDE_CODE_SIMPLE=1 and skips hooks, LSP, plugin sync, attribution, auto-memory, background prefetches, keychain reads, and CLAUDE.md auto-discovery. It still allows explicit context through system prompt flags, --append-system-prompt, --add-dir, --mcp-config, --settings, --agents, and --plugin-dir. This is a reproducibility lever: disable ambient discovery, then require explicit context sources.

## Per-turn Construction

Per turn, Claude assembles the current message history plus a small set of fresh additions. The fresh additions are event-triggered:

- UserPromptSubmit hooks can add additionalContext at the moment a user prompt enters the loop.
- UserPromptExpansion hooks can add additionalContext while a user-typed slash command or MCP prompt is expanding into prompt messages.
- PreToolUse/PermissionRequest can inject context before a tool runs, often alongside allow/deny/ask decisions or updated input.
- PostToolUse/PostToolUseFailure can inject context from a single tool result or failure.
- PostToolBatch injects once for a whole parallel tool batch, after all tool calls resolve and before the next model request.
- PreCompact/PostCompact add or block around compaction; PostCompact receives the summary.
- Session recap (`away_summary`) is generated as a separate no-tools, one-turn fork; the automatic return path appends it as a system status message, while manual `/recap` displays local slash-command output.
- SubagentStart/SubagentStop and Notification hooks can add context around delegated/background work.
- MessageDisplay can transform displayed assistant deltas without changing the stored message, so display context is explicitly separated from transcript context.

This produces a cadence model: reminders and hook context are not always-on. They are attached to lifecycle events or usage triggers. Binary strings explicitly describe PostToolBatch as the once-per-batch point and warn that hook additionalContext should be returned through hookSpecificOutput to inject context once for the whole batch.

Recap is another event-triggered context artifact, but it is not a normal user prompt expansion and not a compaction summary. The generator reuses the current session's saved cache-safe request parameters, makes a single auxiliary model call with `querySource`/`forkLabel` `away_summary`, denies all tools, and skips transcript/cache writes for that auxiliary call. In the automatic path, the successful output is appended to visible/stored messages as a system message subtype `away_summary`, so it can become part of the subsequent conversation context without rewriting older messages.

## Slash-command Expansion

UserPromptExpansion clarifies where slash commands sit in context construction. The raw user input /command args is parsed as a command, then Claude runs UserPromptExpansion hooks before calling getPromptForCommand. Hook input includes expansion_type, command_name, command_args, command_source, and the original prompt string. expansion_type is slash_command for normal prompt commands and mcp_prompt for MCP-backed prompts.

If the hook succeeds with additionalContext, Claude wraps it as hook_additional_context and passes it as hookMessages into the expanded command path. If the command is a normal prompt skill, those hook messages are appended around the generated prompt material before the model query. If the command uses forked context, the hook messages are passed into the forked command execution. If the hook blocks, expansion stops and shouldQuery=false, so the generated prompt never enters the model turn.

This means slash commands are not just user text macros. They are a pre-turn expansion stage with a hookable context seam: command invocation text, generated prompt body, skill permission/model metadata, and expansion-hook context become distinct pieces of the eventual turn.

## System Reminders And Nudges

The existing observation still holds: Claude uses small system-reminder-wrapped harness messages for trigger-gated steering. Examples include todo nudges when the todo list has not been used recently, deferred ToolSearch/MCP manifests, MCP server instructions, skill availability, plan-mode reminders, and context-compression reassurance.

The todo path is especially concrete in binary strings. TodoWrite tool results tell the model that todos were modified and to continue using the todo list if applicable. A separate reminder says the todo list has not been used recently, includes existing todo contents, and frames the nudge as gentle/ignorable. This is a per-turn nudge, not a permanent system-prompt clause.

## Message Shape And Cache Discipline

Tool messages impose strict context shape. A message containing tool_result blocks must contain only tool_result content, and IDs must match the previous assistant tool_use blocks. The loop has a repair path for missing tool_result blocks. This protects context construction from malformed turns after interruption, fork, or partial execution.

Prompt caching appears to drive the stable/dynamic split. The default system prompt can carry stable reusable material, while --exclude-dynamic-system-prompt-sections moves per-machine volatile sections into the first user message. Compaction then rewrites the message buffer client-side when the context window gets too large.

## Design Takeaways

- Separate cache-stable system material from host/session volatile material.
- Treat first-turn context differently from every-turn context; the first user message can carry dynamic sections when cache reuse matters.
- Prefer event-triggered hook/reminder injection over repeating broad guidance every turn.
- Use a whole-batch post-tool seam for context generated by parallel work.
- Keep display transformations separate from stored transcript content.

## Open

<!-- Exact builder function names and ordering; precise trigger predicates/cooldowns for each system reminder; whether hook additionalContext is appended as user content, system-reminder content, or event metadata in every transport path; exact cache breakpoint placement when dynamic sections are excluded. -->

## Feeds

design/bro-harness/bro-harness-hooks.md and design/bro-harness/anthropic-harness.md.
