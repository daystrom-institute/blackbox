---
question_shapes:
  - question_shape: where
    query: Locate the phase decomposer ensemble, foreach implementer, recomposition, whiteboard, gate packet, and no-edit guard artifacts for a no-edit decomposition smoke test.
    scope_hint: system-defaults/workflows/phase-decompose/ensemble-decompose.json system-defaults/workflows/phase-decompose/supervised-impl.json system-defaults/workflows/phase-decompose/recompose.json system-defaults/workflows/phase-decompose/main.json system-defaults/agentic-corpus/packets/phase-decompose system-defaults/phase-decompose/scripts/no-edit-diff-guard.py system-defaults/phase-decompose/scripts/recompose-assertions.py
    known_evidence: file:system-defaults/workflows/phase-decompose/ensemble-decompose.json
---

# Phase Decomposer Ensemble Path Live Smoke

This is a live-test/no-edit phase. Do not edit files.

Acceptance:

- AC-DECOMP-1: The inlet aggregates scout evidence and classifies this assignment as `needs_decompose` when the target context window is intentionally tiny.
- AC-DECOMP-2: The ensemble produces a DAG with sub-units that cover acceptance IDs and fit the target context window.
- AC-DECOMP-3: The foreach supervised implementer path completes no-edit sub-unit assignments and the recomposition council returns a terminal verdict.
