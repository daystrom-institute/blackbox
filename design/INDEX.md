# Design Corpus Index

This directory holds design records for Blackbox. Treat it as a work-tracking
corpus, not as the authority for current runtime behavior. When a design
describes behavior that matters for implementation, verify it against the code,
`PROJECT.md`, and current tests before relying on it.

## Directory Semantics

- [`proposed/`](proposed/) - candidate designs and not-yet-accepted directions.
  These are useful for intent and options, but they are not current behavior.
- [`partial/`](partial/) - in-flight designs or implementation plans where some
  work has landed and some remains. Verify both the doc and code before acting.
- [`archive/`](archive/) - shipped, closed, or historical designs. These are
  useful for provenance and rationale, but newer code or system memories may
  supersede details.

## Current Entry Points

- [Obsidian Document Context Surface](proposed/obsidian-document-context-surface.md)
  - active proposal for making this corpus easier to read in Obsidian through a
  read-only Blackbox context pane.
- [Phase Decomposer](partial/phase-decomposer.md) - active/in-flight large-plan
  decomposition design.
- [Restructure Proposal](partial/restructure.md) - in-flight crate topology
  restructure record.

## Maintenance Notes

- Prefer updating the source design doc over summarizing details here.
- Move documents between `proposed/`, `partial/`, and `archive/` when their
  lifecycle changes.
- Keep this file as a small map. Do not turn it into a full catalog unless the
  browsing friction becomes obvious.
