---
title: "Refactor Tools (Retired MCP Surface)"
brief: "The daemon's refactor/slice/code-nav/macro MCP tools are retired; refactor tooling is harness-native via the bro-harness isolate bindings."
tags:
  - refactor-tools
  - atoms
---
# Refactor Tools (Retired MCP Surface)

The daemon no longer exposes refactor, slice, code-navigation, or macro MCP
tools. The retired surface: `bbox_refactor_*` (6 tools), `bbox_slice_*` (6),
`bbox_code_*` and `bbox_workspace_symbols` (9), and `macro_*` (8), plus the
in-process `refactor_plan` / `refactor_plan_get` capability tools the daemon
used to inject into harness sessions.

Refactor tooling is now harness-native. The bro-harness `isolate` binary and
the in-box cell bindings (`code.*` facts, `java.*` transforms, `edits.*`
mutation choke point, `analysis.*`, `lsp.*`) link the same engine crates
directly, with no daemon reach-back. See the isolate validation recipes in
`PROJECT.md` and the design in `design/bro-harness/refactor-tools-v2.md`.

Agents that only speak MCP (interactive operator sessions, external clients)
direct refactoring by dispatching a harness worker via `bro_exec` /
`bro_resume`, or by consuming a canned atom whose implementation drives the
cell path.

The `bbox-refactor` and `bbox-lsp` crates remain in the workspace as libraries
consumed by the harness bindings; their plan kinds, hash guards, rollback, and
fail-closed LSP behavior are unchanged.
