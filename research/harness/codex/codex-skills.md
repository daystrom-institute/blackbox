---
title: "Codex · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: skills
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - skills
brief: "Codex skills are now extension-owned across catalog, discovery, rendering, list/read tools, invocation accounting, and World State. A deterministic weighted lexical selector runs in bounded shadow mode against eligible prompt-visible skills and records reduction, latency, hit, and rank against actual invocation before any catalog disclosure policy changes."
---

# Codex - Skills

See axis: [Skills](../skills.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md). Code mode now has its own
[metatools finding](codex-metatools.md).

## Finding

The `SKILL.md` authoring and progressive-disclosure model from 0.136.0 remains,
but the runtime has moved into `codex-rs/ext/skills`. The extension owns the
catalog, sources, state, host/orchestrator providers, rendering, list/read tools,
implicit-invocation accounting, and World State projection.

**Confidence: high.** The complete extension and its experiment tests are open
source at the captured revision.

### Bounded shadow selection

Codex now evaluates a deterministic weighted lexical selector on every eligible
turn without changing the model-visible catalog. The experiment:

- truncates the user-input query at 16 KiB;
- returns at most 20 candidates;
- considers enabled, prompt-visible host and orchestrator skills whose actual
  invocation can be observed;
- records catalog size, selected size, query terms, reduction basis points,
  latency, truncation state, and selection status;
- records whether a subsequently invoked skill was selected and at what rank;
- de-duplicates invocation observations per turn.

The selector itself is required to remain deterministic, side-effect free, and
cheap enough for per-turn shadow execution. Selection output is sanitized back
to the eligible catalog before metrics are recorded.

This is an evidence-gathering rollout, not yet an authority mechanism. The
catalog remains visible according to the existing policy while the experiment
measures false negatives and useful reduction.

### Discovery and mutable context

Environment-scoped skill discovery, cached catalog inventory, and parallel
startup now feed extension state. Selected executor skills are projected through
World State, so their instructions can change without rebuilding unrelated
context and can be reconstructed after resume or compaction.

The previous authoring facts remain: file-backed `SKILL.md`, system/user/project
and plugin sources, frontmatter policy, progressive body loading, and optional
MCP dependencies.

## Evidence

- `codex-rs/ext/skills/src/dynamic_skill_selector/weighted_lexical.rs` - cheap
  deterministic selector.
- `codex-rs/ext/skills/src/shadow_selection_experiment.rs` - bounds, metrics,
  and invocation comparison.
- `codex-rs/ext/skills/src/extension.rs` and `world_state.rs` - lifecycle and
  context projection.
- Commits `c100109280`, `2b0b37abb7`, and the preceding skills-service and
  environment-discovery refactors in the captured range.

## Vs the axis

Progressive disclosure now has a measurable pre-deployment step: run a proposed
selector in shadow mode against observed invocation before withholding catalog
entries. This should be treated as an axis-level safety invariant for dynamic
skill selection, not as a Codex-specific ranking preference.

## Open

- The experiment is explicitly temporary. The source does not establish the
  acceptance threshold or eventual production selection policy.
- Invocation observability currently bounds which skill sources can be evaluated
  fairly.
