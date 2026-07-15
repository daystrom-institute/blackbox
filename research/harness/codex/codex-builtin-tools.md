---
title: "Codex · Built-in Tools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: builtin-tools
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - builtin-tools
brief: "Codex retains JSON, freeform-grammar, output-schema, and parallel-safety tool contracts. New model-facing built-ins expose remaining context tokens, request a new context window without resetting environment state, return current UTC time, sleep interruptibly, and temporarily wait for a deferred execution environment; code mode adds a dedicated generated-image egress helper."
---

# Codex - Built-in Tools

See axis: [Built-in Tools](../builtin-tools.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

The earlier tool I/O contract remains: JSON functions, grammar-constrained
freeform tools, optional output schemas, per-tool concurrency classification,
and steering in the tool description. This refresh focuses on newly added
model-facing primitives.

**Confidence: high.** The tool schemas and handlers are open source at the
captured revision.

### Context agency

- `get_context_remaining` has no arguments and returns
  `{ "tokens_left": integer | null }`.
- `new_context` requests a new context window and explicitly preserves
  environment state.

These are context-capacity tools, not durable goal-budget tools. They expose the
state and transition of the active model window while leaving orchestration
budgets to the goal/runtime layer.

### Time and waiting

- `clock.curr_time` returns current UTC time in a structured code-mode-friendly
  result.
- `clock.sleep` supports long but bounded sleeps and is interruptible by new
  input. Sleep state is represented through the extension-owned item lifecycle.
- `wait_for_environment` appears only while a deferred executor environment is
  starting and is replaced by that environment's actual tools when ready.

These tools make waiting explicit and interruptible rather than hiding it in a
blocking tool call.

### Code-mode result helper

`generatedImage(...)` distinguishes an image-generation result from generic
image forwarding. Generic `image(...)` rejects remote HTTP(S) URLs and accepts
data URLs or typed tool image content.

The pre-existing shell, patch, plan, goal, elicitation, MCP resource, batch
agent, plugin, dynamic, and image-view tools remain part of the broader catalog.

## Evidence

- `codex-rs/core/src/tools/handlers/get_context_remaining_spec.rs`.
- `codex-rs/core/src/tools/handlers/new_context_window_spec.rs`.
- `codex-rs/core/src/tools/handlers/current_time.rs`, `sleep.rs`, and
  `wait_for_environment.rs`.
- `codex-rs/code-mode-protocol/src/description.rs` - generated-image helper.

## Vs the axis

The new tools expose two useful surface patterns: model-visible capacity
introspection and interruptible waiting. Both return control-plane facts without
granting new filesystem or network authority.

## Open

- `wait_for_environment` is meaningful only for harnesses with dynamically
  materialized execution environments.
- Durable scheduled wakeups remain distinct from an interruptible sleep inside
  a live turn.
