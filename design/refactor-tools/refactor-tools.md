---
title: "Refactor Tools"
kind: design-hub
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
brief: "Hub for Blackbox structural refactor tooling, refactor atoms, Rust expansion, Java refactor closure, Elixir expansion, and the C# track."
---

# Refactor Tools

This hub groups the designs around structural refactor primitives, compound
refactor runs, macro-assisted synthesis, language-specific expansion, and
refactor atoms.

## Core Refactor Surface

- [AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md)
- [Refactor Compound Runs](refactor-compound-runs.md)
- [Context Clipboard Refactor Primitives](context-clipboard-refactor-primitives.md)
- [Code Macro System](code-macro-system.md)
- [Crate Topology Restructure](restructure.md)
- [AST-Grounded Restructure Execution Plan](restructure-ast.md)
- [Refactor Surface Benchmark](refactor-restructure-benchmark.md)

## Diagnosis

- [Architecture Pathology](arch-pathology.md) — diagnosis workflow that emits
  a reviewable refactor plan-doc, consumed by phase-decompose for execution.
- [Performance Pathology](perf-pathology.md) — sibling diagnosis workflow
  for performance and efficiency smells. Reuses the arch-pathology machinery
  with cost-dimension axis, multi-source evidence, baseline measurements,
  and delta-based acceptance.

## Refactor Atoms

- [Refactor Agents](refactor-agents.md)
- [Refactor Agents - Implementation Skeleton](refactor-agents-impl.md)
- [Atom Capability Runtime](../orchestration/atoms/atom-capability-runtime.md)

## Language Clusters

- [Rust Refactor Tools](rust/rust-refactor-tools.md)
- [Java Refactor Tools](java/java-refactor-tools.md)
- [Elixir Refactor Tools](elixir/elixir-refactor-tools.md)
- [C# Refactor Tools](csharp/csharp-refactor-tools.md)
