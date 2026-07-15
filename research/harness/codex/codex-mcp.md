---
title: "Codex · MCP Tooling"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: mcp
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - mcp
brief: "Codex retains ranked deferred MCP discovery and now adds a process-scoped sanitized stdio tool-catalog cache: 32-entry LRU, 30-minute TTL, identity keyed by server/config/environment/cwd/elicitation capabilities, generation-safe publication, no HTTP or remote-source reuse, and no cached annotations or connection-scoped namespace instructions. Calls still require the live runtime."
---

# Codex - MCP Tooling

See axis: [MCP Tooling](../mcp.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

The 0.136.0 deferred-exposure model remains: ranked tool search, qualified
names, a unified direct/deferred pipeline, and layered approval. The material
new behavior is a **process-scoped warm catalog** that separates reusable tool
definitions from live connection authority.

**Confidence: high.** Cache identity, sanitization, limits, and tests are open
source at the captured revision.

### Cache contract

`McpToolCatalogCache` is a 32-entry LRU with a 30-minute TTL. Its identity
includes server name, a fingerprint of stdio command/args/environment/cwd,
execution environment identity, and client elicitation capabilities. Local
stdio servers without an explicit cwd also include the resolved fallback cwd.

Concurrent refreshes carry monotonically increasing generations. Only the
newest completed fetch may publish, preventing a slower stale list response
from replacing a newer catalog.

### What is deliberately not cached

Before publication, Codex removes:

- connection-scoped namespace/initialize instructions;
- tool annotations that affect approval or parallelism.

HTTP transports are not reused because a canonical resolved-auth identity is
not yet available. Stdio configurations with environment values sourced from a
remote authority are also excluded. Tool execution and authoritative runtime
metadata always come from the live connection.

The result is a latency optimization and early discovery hint, not an execution
or policy cache.

### Runtime unification

MCP runtimes are built before tool-surface planning so direct, deferred, and
code-mode projections derive from the same live runtime objects. Shared MCP
types remain available in code-mode declarations when individual tools are
deferred. Stdio writes are serialized to avoid concurrent framing corruption.

## Evidence

- `codex-rs/codex-mcp/src/tool_catalog_cache.rs` - cache, identity, and
  sanitization contract.
- `codex-rs/core/src/mcp_tool_exposure.rs` - direct/deferred planning.
- `codex-rs/core/src/tools/handlers/tool_search_spec.rs` - search surface.
- Commits `42c5d3c80d`, `1447cee36b`, `44954d1b4b`, and `a6b99ee5c4`.

## Vs the axis

The axis should distinguish **definition reuse** from **connection reuse** and
**execution authority**. A cached schema can improve startup without authorizing
a call, carrying approvals, or pretending a dead server is live.

## Open

- HTTP catalog sharing remains blocked on a safe auth-aware identity.
- The cache is process-scoped, so cross-process workers need a separate design
  if warm catalogs become operationally important there.
