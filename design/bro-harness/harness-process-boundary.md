---
title: "Standalone harness process boundary"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - process-isolation
  - capability-projection
supersedes: "harness-daemon-boundary.md - retires in-process harness execution"
---

# Standalone harness process boundary

## 0. Decision

Every harness-backed provider dispatch runs in its own `bro-harness` process.
`blackboxd` supervises the process but does not link the harness, provider
transports, code-mode runtime, or V8. The V8 isolate remains inside the harness
child for a provider session. The `isolate` binary is a second, independently
executable validation surface over the same harness-owned runtime.

This extraction is a behavior-preserving boundary change. Process separation
is not accepted if it narrows tools, controls, event fidelity, persistence,
environment routing, resume behavior, or failure reporting.

## 1. Runtime topology

```text
blackboxd
  |-- stdin NDJSON: user turns and controls
  |-- stdout NDJSON: Claude-compatible harness events
  |-- HTTP MCP: complete server-filtered daemon tool catalog
  |-- filesystem: $BRO_HOME/harness-sessions/<session>.events.jsonl
  |
  `-- bro-harness child
        |-- provider HTTP/WebSocket transport
        |-- harness built-ins and MCP client
        |-- code-mode and V8 isolate
        `-- supervised shell/tool children

operator/test shell
  `-- isolate binary
        `-- code-mode and V8 isolate, without blackboxd or an LLM
```

The Cargo graph enforces the topology. `blackbox` has no dependency on
`bro-harness` or `bro-capabilities`; therefore V8 cannot enter the daemon
through the harness graph. `bro-harness` remains a workspace member and never
depends on `blackbox`.

## 2. Boundary channels

| Concern | Channel | Contract |
|---|---|---|
| Initial and later user turns | child stdin NDJSON | Claude stream-json user envelope; the initial prompt is removed from argv and sent through this same path |
| Interrupt, redirect, model change, compact | child stdin NDJSON | shared `SessionCommand` maps to handled control/user messages, never acknowledged no-ops |
| Events and results | child stdout NDJSON | exact harness envelope; the daemon uses one ingestion path for event rings, roster updates, supervision, usage, session identity, and terminal errors |
| Durable transcript | event-log JSONL | child receives the task store as `BRO_HOME`; daemon records the same deterministic location |
| Daemon capabilities | streamable HTTP MCP | daemon surface policy and recursion guards remain authoritative |
| Provider identity and credentials | child environment | available to the provider transport, scrubbed from shell grandchildren |
| Project build environment | `--shell-env` | non-secret dedicated lane delivered only to shell children |

## 3. Capability parity

The child receives the complete daemon MCP server, not a curated reconstruction
of selected tools. Qualified names such as `mcp__blackbox__bbox_search` remain
the catalog authority and stay subject to server-side surfaces plus the
dispatch allow/deny filter.

Two historical direct-capability names remain compatibility aliases:

| Flat harness name | Daemon MCP source |
|---|---|
| `corpus_search` | `bbox_corpus_search` |
| `atom_invoke` | `atom_invoke` |

Aliases share the source tool's backend, schema, and policy. They do not replace
or hide the qualified source tool. If the named daemon MCP server is absent,
unreachable, filtered, or does not advertise the source method, the alias is
absent and fails closed.

This rule addresses the central defect in the earlier extraction attempt: a
small hand-curated projection silently removed most `bbox_*` functionality.
Future capability additions belong in the daemon catalog and become available
without a second worker-specific inventory.

## 4. Lifecycle and failure semantics

The daemon launches the child in stream-json mode with `--exit-when-idle`.
Because the initial turn travels over stdin, the child waits for the first
controlled input before entering non-blocking idle drain. It persists after
each completed turn and once more at clean exit.

Process exit is not the only terminal signal. A `result` event with
`is_error=true` marks the task failed and preserves its message even when the
child exits with code zero. A child crash or failed turn cannot unwind the
daemon or a sibling harness process. Cancellation still kills the recorded
child PID, and the waiter drains stdout and stderr before publishing terminal
state.

## 5. Parity acceptance matrix

| Surface | Required evidence |
|---|---|
| Compile boundary | root manifest excludes `bro-harness` and `bro-capabilities`; Cargo graph excludes harness and V8 from `blackbox` |
| Child launch | prompt is stdin-only; cwd, model, effort, tier, schema, dispatch context, tool defaults, shell env, MCP config, and provider env survive |
| Complete capabilities | MCP alias test proves flat aliases plus unrelated qualified catalog tools coexist |
| Control plane | user, interrupt, redirect, set-model, and compact wire mappings |
| Event plane | shared ingestion covers stream throttling, fork rejection, usage, result errors, roster, tail, and system events |
| Persistence | deterministic event-log path and per-turn session persistence |
| Isolation | zero-exit error child fails while a sibling child completes |
| Standalone binaries | `bro-harness --help` starts without daemon state; `isolate --cell` executes a V8 cell in its own process |

## 6. Explicit non-goals

- No in-process rollback mode. A hidden fallback would preserve the coupled
  architecture and let parity regressions escape.
- No second V8 companion process inside a provider session. The harness child
  is already the isolate fault boundary.
- No daemon-restart reconnect protocol in this slice. UDS worker registration,
  replay windows, and adoption can be added later without weakening the
  process and capability contracts above.
- No worker-only curated copy of the daemon tool catalog.
