---
title: "Refactor Tools"
brief: "Structural refactor tooling is harness-native via the bro-harness isolate bindings; the daemon serves no refactor MCP tools."
tags:
  - refactor-tools
---
# Refactor Tools

Structural refactor tooling is harness-native. The daemon exposes no
refactor, slice, code-navigation, or macro MCP tools. The bro-harness
`isolate` binary and the in-box cell bindings (`code.*` facts, `java.*`
transforms, `edits.*` mutation choke point, `analysis.*`, `lsp.*`) link the
same engine crates directly, with no daemon reach-back. See the isolate
validation recipes in `docs/project-guides/validation.md` and the design in
`design/bro-harness/refactor-tools-v2.md`.

Agents that only speak MCP (interactive operator sessions, external clients)
direct refactoring by dispatching a harness worker via `bro_exec` /
`bro_resume`. The caller composes the refactor protocol.

The `bbox-refactor` and `bbox-lsp` crates are libraries consumed by the
harness bindings; they own plan kinds, hash guards, rollback, and fail-closed
LSP behavior.
