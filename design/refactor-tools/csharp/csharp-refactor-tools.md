---
title: "C# Refactor Tools"
kind: design-hub
corpus: blackbox-design
topic:
  - refactor-tools
  - csharp
tags:
  - refactor-tools
  - csharp
brief: "Hub for the C# refactor track — Roslyn-backed plan kinds, source-generator invariants, and atoms scoped to net10.0 / EF Core / Wolverine codebases."
---

# C# Refactor Tools

C# refactor work introduces a third language track alongside Rust and Java.
Unlike the Rust track (compiler-feedback as authority) and the Java track
(jdtls subprocess LSP), C# can lean on **in-process Roslyn** as the semantic
backend, which collapses the dry-run / validation loop into a single workspace
snapshot.

## Docs

- [C# Refactor Expansion](refactor-csharp-expansion.md)

## Crosscuts

- [Refactor Tools](../refactor-tools.md)
- [Rust Refactor Tools](../rust/rust-refactor-tools.md)
- [Java Refactor Tools](../java/java-refactor-tools.md)
- [Refactor Agents](../refactor-agents.md)
- [Refactor Compound Runs](../refactor-compound-runs.md)
