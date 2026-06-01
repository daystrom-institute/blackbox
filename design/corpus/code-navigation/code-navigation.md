---
title: "Code Navigation"
kind: design-hub
corpus: blackbox-design
topic:
  - corpus
  - code-navigation
tags:
  - code-navigation
  - refactor-tools
brief: "Hub for code-navigation and symbolic-exploration designs over indexed project source."
---

# Code Navigation

Code navigation is the corpus layer that turns indexed project source into
symbolic lookup and syntax-grounded exploration surfaces.

## Docs

- [Code Navigation And Symbolic Exploration](code-nav-symbolic-exploration.md)
- [Code Navigation And Symbolic Exploration - Implementation Skeleton](code-nav-symbolic-exploration-impl.md)
- [Code Navigation Depth - Axis 1 (Symbol Resolution)](code-nav-depth-axis1.md)
- [Language/Depth-Aware Chunker Symbol Emission](code-nav-chunker-symbol-emission.md)
- [Semantic UI Component Locator](semantic-ui-component-locator.md)

## Crosscuts

- [Agentic Corpus Platform](../agentic-corpus/agentic-corpus-platform.md)
- [Refactor Tools](../../refactor-tools/refactor-tools.md)
- [Hoisting Java to First-Class Code Navigation](../../refactor-tools/java/java-code-nav.md)
  — adds a jdtls-backed semantic tier (symbol/type resolution) to `bbox_code_*`,
  orthogonal to the syntactic synthesis discussed in the depth-axis docs.
- [Unified Code Synthesis Model](../../refactor-tools/unified-code-synthesis-model.md)
  — macro `probe` operations bind to the code-nav query substrate.
