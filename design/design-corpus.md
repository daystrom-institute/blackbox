---
title: "Design Corpus"
kind: design-hub
corpus: blackbox-design
topic:
  - design-corpus
brief: "Top-level map for the Blackbox design corpus, with topic hubs and lifecycle guidance."
---

# Design Corpus

This directory holds design records for Blackbox. Treat it as a work-tracking
corpus, not as the authority for current runtime behavior. When a design
describes behavior that matters for implementation, verify it against the code,
`PROJECT.md`, and current tests before relying on it.

## Topic Hubs

- [Corpus](corpus/corpus.md) - search, provenance, knowledge, notes, storage,
  code navigation, and corpus-facing assistants.
- [Orchestration](orchestration/orchestration.md) - atoms, agents, workflows,
  supervision, phase decomposition, and runtime handoff.
- [Bro-Harness](bro-harness/bro-harness.md) - the custom headless coding agent:
  transports, tool surface, clipboard, tool chaining, hooks, diagnostics,
  neuralyze. Daemon-independent by invariant.
- [Fleet TUI](fleet-tui/fleet-tui.md) - `bro fleet`, the multi-provider cockpit
  for live-driving many concurrent entrypoint agents in-process.
- [Refactor Tools](refactor-tools/refactor-tools.md) - structural refactor
  machinery, refactor atoms, Rust expansion, and Java gap closure.
- [Integrations](integrations/integrations.md) - Obsidian, Slack, and other
  external user-facing adapters.
- [Surfaces](surfaces/surfaces.md) - MCP, workspace tools, and provider
  transcript read planes.
- [Operations](operations/operations.md) - config, artifacts, bundles, doctor,
  and evented coordination.

## Lifecycle

Lifecycle is now metadata, not the primary filing system:

- `lifecycle: proposed` - candidate designs and not-yet-accepted directions.
- `lifecycle: partial` - in-flight designs or implementation plans where some
  work has landed and some remains.
- `lifecycle: archived` - shipped, closed, superseded, or historical designs.

The old `proposed/`, `partial/`, and `archive/` directories remain for documents
not yet migrated into topic homes.

## Frontmatter schema

The full frontmatter contract — the chassis fields shared by `design/`,
`research/`, and `specs/`, plus the per-corpus vocabulary matrix and the
`topic[]` convention — lives in
[`docs/corpus-frontmatter-schema.md`](../docs/corpus-frontmatter-schema.md).
That doc is the single source of truth for the chassis. This section is a
quick reference for the per-`design/` vocabulary only.

```yaml
# Hub (design-corpus.md, <topic>/<topic>.md)
kind: design-hub
corpus: blackbox-design
topic: [<corpus-root-segment>]            # mirrors the directory path

# Leaf
kind: design
lifecycle: proposed|partial|archived       # or `superseded` (see note)
corpus: blackbox-design
topic: [<corpus-root-segment>, <subdir>]   # mirrors the directory path
tags: [refactor-tools, java, …]            # open-vocabulary; optional
brief: "…"                                 # one-line summary; recommended
superseded_by: "<filename>.md — <rationale>"  # optional
```

**Per-`design/` vocabulary (observed in the current corpus):**

| Field | Hub | Leaf | Notes |
|---|---|---|---|
| `kind` | `design-hub` | `design` | required; the only observed leaf kind. Outlier: `correction-plan` (1 file, `corpus: project-refactor`, pathology tooling) |
| `corpus` | `blackbox-design` | `blackbox-design` | required; matches the directory. Outlier: `corpus: project-refactor` (1 file) |
| `lifecycle` | — | required | one of `proposed`, `partial`, `archived`. Observed extension: `superseded` (5 files), typically paired with `superseded_by:` — see the shared chassis doc for the disposition |
| `topic` | required | required | list, mirrors the directory path minus the corpus root; see the shared chassis doc for the convention |
| `tags` | optional (10 hubs) | optional (45 leaves) | open-vocabulary list of hyphenated lowercase tokens (`refactor-tools`, `java`, `rust`, `pathology`, `mcp`, `atoms`, `integrations`, `slack`, `obsidian`, `lsp`, `jdtls`, `roslyn`, `beam`, `elixir`, `csharp`, `gap-notes`, `whiteboard`, `chunker`, `implemented-atoms`, …) |
| `brief` | recommended | recommended | one-line human summary; what the Obsidian graph previews |
| `date` / `updated` / `revision` | — | optional | ISO-8601 date / human revision note; observed on a minority of leaves |
| `supersedes` / `superseded_by` | — | optional | free-text pointer to a successor or predecessor doc; `superseded_by` is the more common direction |

**Tooling that depends on this.** [`design/list-design-docs.sh`](list-design-docs.sh)
greps `kind: design` + `lifecycle: <wanted>` and prints a `<lifecycle> <path> <title>`
summary. It only sweeps `proposed` and `partial` by default (use the script's
positional argument to narrow further) and refuses other lifecycle values,
so prefer `archived` for retired docs unless `superseded` is necessary for
an in-flight migration.

The full lifecycle arrows for `lifecycle` (`proposed` → `partial` → `archived`)
are documented in **Lifecycle** above.

## Maintenance Notes

- Prefer updating the source design doc over summarizing details here.
- Put new design docs in the topic hierarchy when the topic is obvious.
- Use a descriptive hub-note filename for each hierarchy unit. Avoid generic
  `INDEX.md` files; they create low-signal graph nodes in Obsidian.
