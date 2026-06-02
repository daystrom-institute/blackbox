---
title: "Antigravity · Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: agent-loop
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - agent-loop
brief: "agy CLI loop is server-side, but the public SDK exposes the local harness loop: Conversation.send drains/queues turns, receive_steps records Step objects and compaction indices, receive_chunks yields Thought/Text/ToolCall chunks, LocalConnection streams StepUpdate messages over WebSocket, cancel sends halt_request, and parent idle waits for active subagents to finish."
---

# Antigravity · Agent Loop

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Agent Loop](../agent-loop.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** The loop runs server-side; the client streams and renders step updates. Protobuf: `ToolConfig.GetToolTurnLimit`; `YieldInfo{GetRemainingSteps, GetCompletedStepResponses, Step{GetTool,GetPrompt}}` (yield/step accounting); `InvokeSubagentToolConfig` with fast/heavy model tiers; `disable_loop_detection` opt-out (loop detection in `jetski/cortex/utils/loop_detection.go`, "[ignoring loop detection]"). CHANGELOG v1.0.2: the default 60s interaction timeout was restricted to subagents only.

**Evidence.**
- `YieldInfo{GetRemainingSteps,GetCompletedStepResponses}`
- `disable_loop_detection` (agent config varint), "[ignoring loop detection]"
- CHANGELOG v1.0.2: 60s timeout scoped to subagents

**Vs the axis.** Confirms turn/step limits + a **server-side loop-detection** guard (a robustness/loop axis crossover). **Divergence:** no autonomous client loop — the client is a step-renderer.

## SDK/local harness update (2026-06-02)

The public SDK turns this from a string-only finding into a source-grounded one for the SDK surface. Agent.__aenter__ constructs hook, MCP, tool, connection, conversation, and trigger runners before returning the active agent. Conversation.send either drains the previous turn or waits for idle, records a turn-start index, and sends the user message through the connection. Conversation.receive_steps appends Step objects, records compaction step indices, accumulates usage metadata, and enforces a max history length. Conversation.receive_chunks is the model-facing stream abstraction: it yields Thought, Text, and deduplicated ToolCall chunks.

LocalConnection is the transport adapter for the Go local harness. It serializes InputEvent messages over WebSocket and consumes OutputEvent/StepUpdate messages into the Python queue. A turn is not idle while parent execution is active, while tool calls are pending, or while SDK-tracked subagents remain active. cancel sends an InputEvent with halt_request=true; disconnect runs session-end hooks, closes the socket/stdin, waits, and escalates to TERM/KILL only if the local harness process does not exit.

The closed agy CLI still appears to delegate the full production loop to cortex, so SDK semantics should not be blindly promoted to remote server semantics. The SDK does, however, establish Antigravity's intended loop vocabulary: stateful conversation, step stream, tool call/result pairs, compaction steps, subagent-aware idle, and explicit cancellation.

## Open
<!-- Server step-loop semantics; end-of-turn signaling over gRPC. -->
