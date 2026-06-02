---
title: "Axis: Compaction"
kind: research-axis
corpus: blackbox-research
track: harness
axis: compaction
topic:
  - harness
  - compaction
brief: "Cross-harness invariant model for the compaction axis: how a harness shrinks a near-full context window — the summarizer prompt(s), the trigger math (proactive pre-sampling vs reactive overflow), the split point, the verbatim-retention tail, server-side vs client-side transforms, and fit-trimming so the compaction request itself fits. Sibling of context-management (assembly is what goes in; compaction is what comes out). Synthesis of the per-subject compaction cells."
---

# Axis: Compaction

> **Scope.** The shrink operation: when the window fills, how does the harness
> reclaim space while preserving enough to continue? Covers the summarizer
> prompt, the trigger, the split, and the rebuild. Sibling of
> [context-management](context-management.md) — that axis is what *enters* the
> window; this axis is what is *removed* from it.

## The dimension

Compaction is where a harness's understanding of "what matters in a
conversation" becomes explicit — encoded in a summarizer prompt and a retention
policy. The shape differs sharply by transport: a stateless Anthropic-family
client must rewrite its own local buffer (`[summary, ...tail]`), while a
Responses/OAuth path may offload to a server-side transform returning an
encrypted compaction item. Both must decide *when* to fire and *what* to keep
verbatim.

## Questions a finding must answer

- **Where does it run?** Client-side buffer rewrite, or server-side transform?
  Auth-mode-gated?
- **The summarizer prompt(s).** How many? Selected how? Section skeleton?
  Continuing-session vs ran-out-of-context framing?
- **Trigger math.** Proactive (pre-sample before a turn that would overflow) or
  reactive (compact on overflow)? Threshold constants? Token-budgeted?
- **Split point & retention tail.** How is the verbatim tail sized/placed?
- **Fit-trimming.** Does the compaction request itself get trimmed to fit the
  window?
- **Fallback.** Inline local summarizer when remote compaction is unavailable?

## Convergence / divergence

| Subject | Where | # prompts | Trigger | Retention tail | Cell |
|---|---|---|---|---|---|
| Claude | client | _TBD_ | _TBD_ | _TBD_ | [claude](claude/claude-compaction.md) |
| Codex | server (OAuth) + inline fallback | _TBD_ | proactive pre-sample | token-budgeted | [codex](codex/codex-compaction.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-compaction.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-compaction.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is the two-prompt split (continuing-session vs ran-out-of-context) a Claude
  idiom, or convergent across harnesses?
- Proactive pre-sampling vs reactive overflow: which is the modern norm?

## Codex-lens extensions

- **Implementation-variant dispatch** — a harness may pick among inline /
  server-side / streaming compaction by provider + feature flag; each has
  different retry/token budgets and mid-turn vs standalone semantics.
- **Post-compact history shape** — a flag (e.g. inject-before-last-user-message
  vs do-not-inject) changes what history the model sees *after* compaction; model
  the post-compact shape, not just the trigger.
- **Rollback ≠ compaction** — history rewind/rollback (trim N turns at a
  boundary) is a *sibling* mechanism owned by
  [session-lifecycle](session-lifecycle.md): it erases backward, whereas
  compaction summarizes forward.

## Feeds

`design/bro-harness/compaction-canonical-anthropic.md` (Claude-side model),
`design/bro-harness/brodex-compaction.md` (Codex/Responses model),
`design/bro-harness/brodex-compaction-followons.md`.
