---
title: "Claude - Hooks"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: hooks
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - hooks
brief: "Claude Code 2.1.160 hooks are a broad harness-level extension API, not just pre-tool shell hooks. UserPromptExpansion specifically fires when a user-typed slash command or MCP prompt expands into prompt messages; input includes expansion_type, command_name, command_args, command_source, and original prompt, matched by command_name. It can block expansion or add context, but its hook-specific output only supports additionalContext."
---

# Claude - Hooks

> Evidence: current ~/.claude/settings.json, claude --help, ~/.claude/cache/changelog.md, focused strings over /Users/invidious/.local/share/claude/versions/2.1.160, and live RTK PreToolUse behavior in this session. The event list and return fields are binary-string/user-doc literals; exact internal call graph remains lower confidence.
See the axis: [Hooks](../hooks.md) - snapshot: [Claude 2.1.160](claude-2.1.160.md).

## Event Catalogue

The embedded hook documentation table and adjacent executor strings expose the broad event catalogue:

- PermissionRequest: runs before an interactive tool permission prompt; matcher is the tool name.
- PermissionDenied: can request retry behavior after denied permission.
- PreToolUse: runs before a tool; can block, allow/deny/ask, and update tool input.
- PostToolUse: runs after a successful tool.
- PostToolBatch: runs once after every tool call in a batch has resolved, before the next model request.
- PostToolUseFailure: runs after a tool fails.
- Notification: runs on notification events.
- UserPromptSubmit: runs when the user submits a prompt; can block.
- UserPromptExpansion: runs when a user-typed slash command or MCP prompt expands into prompt messages; matcher is command_name.
- SessionStart and SessionEnd: session lifecycle hooks.
- SubagentStart and SubagentStop: subagent lifecycle hooks.
- PreCompact and PostCompact: before/after manual or automatic compaction; PreCompact can block compaction, PostCompact receives the summary.
- Setup: repository setup/maintenance hook event.
- ConfigChange: can block settings/config changes or deletion.
- MessageDisplay: can transform or hide assistant message text as it is displayed.

The current local settings file proves the normal configuration shape: hooks is an object keyed by event name; each event has matcher entries, each matcher has hooks. This host has a PreToolUse matcher for Bash that runs the command hook rtk hook claude. CLI --help also exposes --include-hook-events for stream-json output, which can include hook lifecycle events in machine-readable transcripts.

## UserPromptExpansion

UserPromptExpansion is not generic natural-language prompt rewriting. The implementation calls runUserPromptExpansionHook only in the prompt slash-command path, before executing a prompt command. The hook input is JSON with expansion_type, command_name, command_args, command_source, and prompt. expansion_type is slash_command for normal prompt commands and mcp_prompt for MCP-backed prompts. prompt is the original user-visible command string, for example /name args.

The hook metadata says this event runs when a user-typed slash command expands into a prompt. Exit code 0 shows stdout to Claude; exit code 2 blocks expansion and shows stderr to the user; other exit codes show stderr to the user only. The matcher field is command_name.

The hook-specific JSON schema is deliberately narrow: hookEventName=UserPromptExpansion plus optional additionalContext. Unlike UserPromptSubmit, it has no sessionTitle or suppressOriginalPrompt field. Unlike PreToolUse, it cannot mutate command args through updatedInput. It can add context, block, or prevent continuation through the common hook protocol.

Call-site behavior: the slash command path runs UserPromptExpansion before getPromptForCommand and before forked-command dispatch. additionalContext is wrapped as a hook_additional_context attachment with hookName=UserPromptExpansion and passed as hookMessages into the command execution path. For blocked expansion, Claude emits a warning containing the hook block reason and the original prompt, sets shouldQuery=false, and does not expand the command.

## Return Protocol

Hook JSON can return user-visible and model-visible effects. Binary strings and changelog entries identify these fields:

- systemMessage: display a message to the user.
- decision: block for PostToolUse, Stop-like, and UserPromptSubmit semantics; deprecated for PreToolUse in favor of hookSpecificOutput.permissionDecision.
- hookSpecificOutput.additionalContext: inject text back into model context; for UserPromptExpansion this is the only hook-specific output field.
- hookSpecificOutput.permissionDecision: allow, deny, or ask for PreToolUse/PermissionRequest-style decisions.
- hookSpecificOutput.permissionDecisionReason: explanation for the permission decision.
- hookSpecificOutput.updatedInput: modified tool input for PreToolUse; changelog notes it can combine with ask.
- updatedToolOutput: preferred PostToolUse output replacement path; older additionalContext replacement is noted for MCP tools only.
- terminalSequence: emit desktop notifications, title changes, bells, and similar terminal effects.
- reloadSkills: SessionStart can rescan skill directories so newly installed skills are available in the same session.
- hookSpecificOutput.sessionTitle: SessionStart can set the session title.

Exit-code blocking is still present in user-facing behavior: changelog entries mention hook blocking errors on exit code 2 and improved surfacing of stderr. PostToolUse has continueOnBlock so a rejection reason can be fed back to Claude while the turn continues. Tool hook timeout changed from 60 seconds to 10 minutes. Stop hooks that repeatedly block are capped after 8 consecutive blocks unless CLAUDE_CODE_STOP_HOOK_BLOCK_CAP overrides it.

## Policy And Distribution

Hooks are harness-executed, not model-executed. The model sees hook results as feedback/context, while hook processes run outside the model loop. Claude supports command hooks, HTTP hooks with managed URL/env allowlists, plugin-provided hooks via hooks.json, and hook frontmatter on agents, skills, and slash commands. Managed settings can disable all hooks, allow only managed hooks, or block customization surfaces such as hooks/skills/agents/MCP.

## Design Takeaways

- Claude's hook surface is not a small lifecycle callback set; it is a policy, context-injection, display, setup, and plugin extension API.
- PreToolUse/PermissionRequest is the closest equivalent to a programmable permission guardian.
- MessageDisplay and terminalSequence make hooks part of presentation as well as execution.
- SessionStart reloadSkills is a useful pattern for bootstrapping skills without restart.

## Open

- Exact JSON schema per event beyond the mined hookSpecificOutput union, especially SubagentStart/SubagentStop, Setup, ConfigChange, and MessageDisplay inputs.
- Ordering and merge behavior when multiple hooks match.
- Exact timeout/error behavior for HTTP and prompt-based hooks.
- Whether --include-hook-events reports all internal hook events or only stream-json-safe summaries.

## Feeds

design/bro-harness/bro-harness-hooks.md and design/bro-harness/backlog-hooks-catalog-metadata.md.
