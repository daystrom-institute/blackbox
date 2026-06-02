---
title: "Bro-Harness Transports — Spec"
kind: spec
corpus: blackbox-spec
domain: bro-harness
spec: transports
topic:
  - specs
  - bro-harness
  - transports
status: draft
sources:
  - "research:research/harness/transport.md"
  - "research:research/harness/robustness.md"
  - "vendor:Anthropic Messages API"
  - "vendor:OpenAI Responses API"
  - "vendor:Mistral chat-completions API"
supersedes: null
last_reviewed: "2026-06-02"
---

# Bro-Harness Transports — Spec

> **STATUS: `draft` stub.** Skeleton + source pointers only; clauses not yet
> mined. Pick this up per the backfill shape in
> [the domain charter](bro-harness-spec.md).

The normative contract for how `crates/bro-harness` talks to each backend, and
the common output envelope every backend emits.

## Scope & backends

| Backend | Providers | Wire contract |
|---------|-----------|---------------|
| Anthropic Messages | GLM, DeepSeek | SSE stream, `stop_reason`, `pause_turn`, cache control. |
| OpenAI Responses | Brodex (Codex/ChatGPT backend) | Responses API; WebSocket/SSE. |
| openai-chat | VibeBh (Mistral) | chat-completions. |
| **Output envelope (common)** | all | the Claude **stream-json** envelope on stdout — the sole daemon↔harness contract. `[derived]` |

Transport + credentials are selected per provider via env in
`brofile::resolve_provider_env`; binary on PATH or `BRO_HARNESS_BIN`.

## Clauses to specify

- **Retry / backoff** — jittered exponential backoff; honor `Retry-After`
  (cf. commit `831974b`). `[vendor]` `[research]`
- **SSE idle timeout & mid-stream recovery** — idle-timeout detection; retryable
  mid-stream read errors (cf. commit `95f5d6b`). `[vendor]` `[research]`
- **Cache TTL** — extended 1h cache TTL on the Anthropic transport
  (cf. commit `831974b`). `[vendor]`
- **Feature flags / beta gates** — effort, reasoning budgets, token-efficient
  tools, interleaved thinking; per-backend header mapping. `[vendor]` `[research]`
- **Role-alternation repair** — enforce valid role sequences across backends. `[vendor]`
- **Channel fallback** — WS → SSE → HTTP where applicable. `[research]`
- **Output envelope fidelity** — every backend's stream maps to the same
  stream-json envelope the daemon consumes. `[derived]`

## Conformance (to be wired)

| Clause | code anchor | intent (design) | evidence (research) |
|--------|-------------|-----------------|---------------------|
| _stub_ | `crates/bro-harness/src/` (transport modules), `src/orchestration/brofile.rs` (`resolve_provider_env`) | `design/bro-harness/anthropic-harness.md`, `brodex-responses-deep-dive.md`, `brodex-websocket-transport.md`, `backlog-transport-polish.md` | `research/harness/transport.md`, `research/harness/robustness.md` |

Canonicalization commits to mine: `831974b` (cache TTL + jittered backoff),
`95f5d6b` (SSE idle timeout + retryable mid-stream), `efc82bf` (server-tool
blocks + pause_turn), `2a63162` (canonical Anthropic compaction + API robustness
roadmap), `f3ad3fc` (reconcile docs to landed state).

## Open

<!-- TODO(spec): split per-backend clauses where the contract diverges; pull the
     exact header/flag mappings from anthropic-harness.md (31K) and the brodex
     deep-dives; complete Conformance; reconcile against the bro-harness-residuals
     doc for known divergences. -->
- Which robustness clauses are Anthropic-only vs cross-backend?
- Does VibeBh (openai-chat) inherit the Responses robustness model or need its
  own clauses? (see the vibebh allocator/usage thread.)
