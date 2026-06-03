---
title: "Bro-Harness"
kind: design-hub
corpus: blackbox-design
topic:
  - bro-harness
brief: "Nav hub for the bro-harness design cluster: the custom headless coding agent that speaks provider APIs directly behind one Transport, runs its own tool loop, and emits the Claude stream-json envelope. Top-level abstraction — daemon-independent by invariant. Sorts the cluster into shipped as-built records and a proposed backlog."
---

# Bro-Harness

`bro-harness` (`crates/bro-harness`, `crates/bro-tools`) is the custom headless
coding agent that speaks provider APIs directly behind one `Transport` interface,
runs its own tool-calling loop, and emits the Claude stream-json envelope so it
slots into the existing dispatch seam (GLM/DeepSeek on the Anthropic transport,
Brodex on OpenAI Responses). See `PROJECT.md` → "Provider & Agent Surfaces" for
routing facts.

**Top-level abstraction, not an orchestration sub-topic.** By invariant the
harness shares *code* with the daemon (workspace crates like `bro-tools`) but
**never a runtime dependency** — no MCP/RPC backchannel. It runs with the daemon
down; the only daemon↔harness contract is the stdout stream-json envelope.

This page is the **nav waypoint** — start here, then follow a link. Per-feature
detail lives in each linked doc; this hub keeps only the sort.

## Shipped (as-built records)

The built core. Each is an `archived` as-built record; residual work, where any,
points to a backlog doc.

- [Custom provider harness](anthropic-harness.md) — three transports, agent loop,
  SSE streaming, model-keyed compaction, bidirectional session/control protocol,
  deferred tiering, recursion guard.
- [Tool surface](bro-harness-tool-surface.md) — the built-in subset: shell
  quartet, `file_read`/`content_search`/`glob`, `todo_write`.
- [Clipboard (`clip_*` registers)](bro-harness-clipboard.md) — the nine-tool
  settled-ref register store on the `side` spine.
- [Tool chaining (the ref ABI)](bro-harness-tool-chaining.md) — Stages 1–2:
  settled refs + `kind`-tagged producers/consumers.
- [Hooks & nudges](bro-harness-hooks.md) — system-prompt split, hook seam,
  delivery, Nudger v1 + four rules.
- [Diagnostics (window-0)](bro-harness-diagnostics.md) — the instant/error-tier
  MVP (`bro-lsp` + per-mutation rider); upper tiers deferred.

## Backlog (proposed — pick this up)

- [Transport & tool polish](backlog-transport-polish.md) — MCP connection
  pooling, `codex_auth` retry wrapping, deferred-manifest trimming, web_search
  fallback, structured output, RTK output compaction (← thread-ca160aa2 item-5 +
  anthropic open-questions).
- [Tool chaining Stage 3](backlog-tool-chaining-stage-3.md) — pending refs =
  Task; gated on an async producer existing.
- [Hooks catalog-metadata channel](backlog-hooks-catalog-metadata.md) — v2
  rule-source; gated on the adoption loop.
- [Diagnostics check & truth tiers](backlog-diagnostics-truth-tiers.md) — flycheck
  lints + orchestrator-owned truth tier + the `bro-lsp`/`src/lsp` fork.
- [Per-call privilege escalation](backlog-per-call-escalation.md) — Codex-style
  escalate+justification; gated on unifying the privilege model.
- [Neuralyze (rewind + carry a message)](bro-harness-neuralyze.md) — fully
  unbuilt: checkpoint substrate, context rewind, file inverse-diff journal.
- [NARF capability library and prepared scripts](narf-capability-library.md) —
  proposed authoring-layer middle tier: session-local helpers, decay-managed
  reusable functions, capability scout, and prepare-before-run script refs.

## Cluster conventions

- The async/temporal layer (sessions, checkpoints, pending refs) is
  **harness-owned**, never behind MCP; MCP tools stay synchronous unary.
- Shares **code** with the daemon, never a **runtime** dependency; capabilities
  the harness needs (LSP sessions, etc.) are shared by extracting into a linked
  crate, not by calling a daemon service.
- The `side` persistence spine is the keystone — clipboard, nudge ledger, todos,
  and (future) neuralyze checkpoints all ride it; nothing stateful needs new
  persistence machinery.
- Privilege lives in `SafetyPolicy` + the brofile allow/deny layer. Nudges steer,
  they never gate; neuralyze rewinds, it never escalates privilege.
- Session-scoped state only — no cross-session / cross-bro sharing.
- Provider-agnostic ambient text uses **bare** tool names (`bbox_note`, not
  `mcp__blackbox__bbox_note`); FQDN surfacing is a per-CLI concern.
