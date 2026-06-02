---
title: "Claude - Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: agent-loop
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - agent-loop
brief: "Claude Code 2.1.160's agent loop: stateless Messages API turns, model-directed parallel tool batches, per-tool PostToolUse hooks that may run concurrently, a PostToolBatch hook fired once after the full batch resolves and before the next model request, tool_use/tool_result pairing validation/repair, pause_turn resume, max-turns for print mode, and per-turn context injection through hook additionalContext/system reminders."
---

# Claude - Agent Loop

> Evidence: direct observation of the running 2.1.160 harness, current claude --help, focused binary strings over /Users/invidious/.local/share/claude/versions/2.1.160, and prior bro-harness api-robustness/compaction mines. See [snapshot](claude-2.1.160.md).

See the axis: [Agent Loop](../agent-loop.md).

## Turn Shape

Claude Code uses the stateless Anthropic Messages API: each model request sends the current system/context material plus the conversation messages. The client owns history shape, compaction, tool-result repair, and context injection. In print mode, --max-turns caps agentic turns; the binary describes this as a maximum number of API round trips before stopping.

Stop reasons include end_turn, tool_use, max_tokens, and pause_turn. pause_turn is resumable for server-side agentic flows: the client should send the prior user message plus assistant response again, not inject a new user message like Continue. This keeps resume semantics in the API protocol rather than in prompt text.

## Tool Batches

Parallelism is model-directed. The harness instructs the agent to put independent tool calls in the same assistant message and to wait when later calls depend on earlier outputs. The executor then runs independent calls concurrently.

The binary exposes the hook boundary around tool batches. PostToolUse fires once per tool and may run concurrently for parallel tool calls. PostToolBatch fires exactly once after every tool call in a batch has resolved, before the next model request. That makes PostToolBatch the clean per-turn injection point for whole-batch feedback/context.

Message shape is enforced. If the last message contains tool_result blocks, it must contain only tool_result content, and tool_result IDs must match the tool_use IDs from the previous assistant message. Binary strings show an ensureToolResultPairing repair path for missing tool_result blocks, so interrupted or forked turns are padded instead of leaving orphaned tool_use blocks.

## Per-turn Context Seam

Claude does not appear to rebuild all steering every turn. It keeps stable prompt/tool/skill material in cheaper persistent positions, then injects small event-triggered context at loop boundaries. The concrete seam is: user prompt or tool batch finishes, hooks/system reminders may add context, then the next Messages API request is assembled.

Known per-turn/context injection sources:

- UserPromptSubmit hook output can add additionalContext, set sessionTitle, and suppress the original prompt in block messages.
- PreToolUse and PermissionRequest hooks can decide allow/deny/ask, mutate input, and add context before execution.
- PostToolUse and PostToolUseFailure can add context tied to a specific tool outcome.
- PostToolBatch can add context once for the whole resolved tool batch.
- SessionStart can add additionalContext and initialUserMessage for the first turn.
- Setup, SubagentStart, Notification, and MessageDisplay also have hook-specific context/display effects.
- Trigger-gated system reminders add todo nudges, deferred tool manifests, MCP instructions, skill manifests, plan-mode reminders, and similar harness messages.

## Interrupts, Forks, And Steering

On interrupt, prior mines show role-alternation repair and tool-result padding so a half-finished dispatch never orphans a tool_use. The focused binary pass adds fork-specific evidence: if a fork directive references an assistant message with no tool_use blocks, the harness logs that condition and falls back to synthetic content; otherwise it can generate padding tool_result blocks. The invariant is the same: every assistant tool_use must be paired before the next request.

The precise ordering of operator steering that arrives mid-turn is still not fully characterized. Changelog entries indicate steering messages can be lost while a subagent is working and were fixed, and that async agents/bash commands can wake the main agent. That suggests steering is queued/wakeful, not simply appended raw to the current in-flight API request.

## Design Takeaways

- The per-turn boundary is not each tool; it is the whole tool batch before the next model request.
- Tool-result pairing is a hard transport invariant, and the client repairs/pads around interrupts and forks.
- Whole-batch hook context is a better low-bloat injection point than repeating reminders in every turn.
- --max-turns is an API-round-trip guard for non-interactive runs, separate from model-visible task planning.

## Open

<!-- Exact core loop call graph; exact default iteration/max-turn constants; ordering of parallel tool results in the messages array; whether PostToolBatch output is appended before or after tool_result blocks in every transport mode; exact mid-turn steering queue semantics. -->

## Feeds

design/bro-harness/brodex-agent-loop-learnings.md and design/bro-harness/anthropic-harness.md.
