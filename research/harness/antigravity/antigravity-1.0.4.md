---
title: "Antigravity CLI (agy) - 1.0.4 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: antigravity
version: "1.0.4"
platform: macos-aarch64
captured: "2026-06-02"
supersedes: null
replaces: gemini
status: enriched
topic:
  - harness
  - antigravity
brief: "Point-in-time snapshot for Antigravity CLI (agy) 1.0.4, enriched with the public google-antigravity SDK/localharness source at f74a23f plus local CLI state/logs. The SDK makes the loop, tools, hooks, policies, MCP, skills, persistence, and subagent contract inspectable; the standalone CLI still has opaque server-side cortex behavior."
---

# Antigravity CLI (agy) - 1.0.4 (snapshot)

> **Replaces the deprecated Gemini CLI subject.** agy is Google's terminal
> coding agent going forward. It still uses the ~/.gemini/ namespace, so the
> lineage remains visible in config/state paths.

This snapshot is no longer just README/changelog reconnaissance. It combines:

- Installed CLI: /Users/invidious/.local/bin/agy, agy --version = 1.0.4,
  Mach-O 64-bit arm64, about 135 MB.
- Public SDK: google-antigravity/antigravity-sdk-python, cloned at
  f74a23fc5f4026129a5b4498ce652d7d6018e23f on 2026-06-02. The SDK is the
  strongest evidence for the local harness API: Agent, Conversation,
  LocalConnection, hooks, policies, built-in tools, MCP adapters, triggers,
  skills paths, and generated localharness_pb2 message types.
- Local CLI state: ~/.gemini/antigravity-cli/ includes settings, keybindings,
  cache/project mapping, SQLite conversation stores, brain JSONL transcripts,
  implicit trajectory protos, and logs. Current ~/.gemini/config/mcp_config.json
  is present but empty on this host.
- Binary strings: the closed agy binary exposes Google-internal package names,
  protobuf names, prompt template paths, local store code paths, subagent labels,
  cortex/trajectory vocabulary, and Cloud Code endpoint names. These are
  high-signal but not source-level proof of remote server behavior.

## Architecture Split

There are two related surfaces:

- **CLI/cortex surface.** The standalone agy binary behaves as a thin client
  around Google's backend. Local logs show model discovery against
  daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels,
  propagation of a selected model label, creation of a cascade trajectory, and a
  streamed conversation update. The exact remote turn scheduler, compactor,
  retry loop, prompt assembler, and model router remain opaque.
- **SDK/localharness surface.** The Python SDK launches a Go local harness and
  talks to it over WebSocket. It exposes the agent loop as Python classes and
  generated proto messages. For SDK-backed axes, confidence can be high because
  the source declares the tool list, hook dispatch, policy precedence, MCP
  bridge, conversation lifecycle, compaction markers, and subagent accounting.

## Local State

- ~/.gemini/antigravity-cli/settings.json currently only sets
  enableTelemetry=false.
- ~/.gemini/antigravity-cli/keybindings.json includes subagent actions such as
  subagent.approve_fast and subagent.jump_to_waiting, plus view toggles.
- ~/.gemini/antigravity-cli/conversations/<uuid>.db contains SQLite tables
  trajectory_meta, steps, gen_metadata, executor_metadata, parent_references,
  trajectory_metadata_blob, and battle_mode_infos.
- ~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/ contains typed
  JSONL transcripts. The observed transcript used USER_INPUT,
  CONVERSATION_HISTORY, PLANNER_RESPONSE, LIST_DIRECTORY, and VIEW_FILE events
  with tool call names such as list_dir and view_file.

## Axis Checklist

| Axis | Leaf | Status | Confidence posture |
|---|---|---|---|
| Transport & Feature Flags | [antigravity-transport](antigravity-transport.md) | enriched | high for SDK/localharness, medium for CLI/cortex |
| Robustness | [antigravity-robustness](antigravity-robustness.md) | enriched | high for SDK client error handling, medium for remote retry semantics |
| Compaction | [antigravity-compaction](antigravity-compaction.md) | enriched | high for SDK markers/hooks, medium for CLI compactor internals |
| Agent Loop | [antigravity-agent-loop](antigravity-agent-loop.md) | enriched | high for SDK loop plumbing, medium for CLI server loop |
| Context Management | [antigravity-context-management](antigravity-context-management.md) | enriched | high for SDK instruction/tool filtering, medium for CLI prompt templates |
| Built-in Tools | [antigravity-builtin-tools](antigravity-builtin-tools.md) | enriched | high for SDK built-ins/protos, medium for closed CLI tool docs |
| MCP Tooling | [antigravity-mcp](antigravity-mcp.md) | enriched | high for SDK MCP bridge/filtering, medium for CLI config migration |
| Subagents | [antigravity-subagents](antigravity-subagents.md) | enriched | high for SDK lifecycle, medium for server-side specialization |
| Hooks | [antigravity-hooks](antigravity-hooks.md) | enriched | high for SDK hook API, medium for extra CLI hook runners |
| Skills | [antigravity-skills](antigravity-skills.md) | enriched | high for SDK Agent Skills paths, medium for CLI plugin schema |
| Privilege Approvals | [antigravity-privilege-approvals](antigravity-privilege-approvals.md) | enriched | high for SDK policies, medium for standalone CLI sandbox internals |
| Session Lifecycle | [antigravity-session-lifecycle](antigravity-session-lifecycle.md) | enriched | high for SDK persistence and local SQLite shape |
| Memory Persistence | [antigravity-memory-persistence](antigravity-memory-persistence.md) | enriched | high for session persistence, medium for CLI knowledge/memory semantics |
| Modes & Personas | [antigravity-modes-personas](antigravity-modes-personas.md) | enriched | high for SDK instruction/persona API, medium for CLI modes |
| Planning & Goals | [antigravity-planning-goals](antigravity-planning-goals.md) | enriched | medium; planning artifacts visible, durable goal contract not proven |

## Remaining Gaps

- The exact CLI/cortex wire contract is still not source-confirmed beyond local
  logs, strings, and generated proto names.
- Binary strings reveal prompt template names and some embedded guidance, but the
  research corpus should summarize idioms rather than copy proprietary prompt
  prose into design or shipped code.
- CLI plugin, rules.json, hooks, and MCP schemas need live populated examples
  from a configured host or official schema docs.
- The SDK and standalone CLI likely share concepts but are not identical
  surfaces. Leaves below call out where a claim is SDK-only versus CLI-observed.
