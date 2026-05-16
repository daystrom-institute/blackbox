---
title: "Elixir Refactor Tools"
kind: design-hub
corpus: blackbox-design
topic:
  - refactor-tools
  - elixir
tags:
  - refactor-tools
  - elixir
brief: "Hub for Elixir refactor expansion — atom-tag dispatch decomposition, GenServer concern extraction, facade delegation, and BEAM-specific invariants."
---

# Elixir Refactor Tools

Elixir refactor work centers on a pattern that doesn't appear in the Rust or
Java surfaces: multi-clause function dispatch on a leading atom tag is the
canonical god-module shape on BEAM. The Elixir toolsuite layers Elixir-specific
plan kinds on top of the shared refactor substrate (parse-validation, hash
pinning, transactional apply, compound runs) and reuses the Rust/Java taxonomy
where it transfers cleanly.

## Docs

- [Elixir Refactor Expansion](refactor-elixir-expansion.md)

## Crosscuts

- [Refactor Tools](../refactor-tools.md)
- [Rust Refactor Tools](../rust/rust-refactor-tools.md)
- [Java Refactor Tools](../java/java-refactor-tools.md)
- [Refactor Agents](../refactor-agents.md)
