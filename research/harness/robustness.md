---
title: "Axis: Robustness"
kind: research-axis
corpus: blackbox-research
track: harness
axis: robustness
topic:
  - harness
  - robustness
brief: "Cross-harness invariant model for the robustness axis: the behaviors that separate a production API client from a naive one — retry/backoff with jitter and Retry-After, in-band mid-stream error recovery, idle timeouts, pause_turn/resume, context-overflow recovery, role-alternation repair on interrupt, and spurious-stop detection. Synthesis of the per-subject robustness cells."
---

# Axis: Robustness

> **Scope.** How the harness survives a hostile network and a stateful, fallible
> provider. The error-handling, retry, and recovery behavior layered over the
> [transport](transport.md). Not the happy-path streaming (that is transport) —
> the failure paths.

## The dimension

Robustness is mostly invisible until it isn't. The mature harnesses encode hard-
won recovery idioms: they never launder a mid-stream error into a fake "success"
turn, they repair role alternation after an interrupt so a `tool_use` is never
orphaned, and they distinguish transient from permanent failures. This axis
catalogues those idioms so bro-harness can match them deliberately.

## Questions a finding must answer

- **Retry & backoff.** Capped exponential? Jittered? Honors `Retry-After`
  (seconds *and* HTTP-date forms)? Which statuses are classified retryable?
- **In-band stream errors.** An `overloaded_error` arriving *after* the 200
  stream opened — captured, classified, retried? Or silently swallowed?
- **Idle / stall handling.** SSE idle timeout? Mid-stream read retry?
- **Pause / resume.** `pause_turn` (server tool hitting an iteration limit) —
  resumed, or mapped to a generic stop and terminated?
- **Context overflow.** On window overflow — compact-and-retry, or hard fail?
- **Interrupt repair.** Role-alternation repair; tool-result padding so an
  interrupted dispatch never orphans a `tool_use`.
- **Spurious-stop detection.** Empty-output / outstanding-async turn-end
  diagnostics.

## Convergence / divergence

| Subject | Backoff | In-band retry | Pause/resume | Overflow recovery | Cell |
|---|---|---|---|---|---|
| Claude | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [claude](claude/claude-robustness.md) |
| Codex | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [codex](codex/codex-robustness.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-robustness.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-robustness.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is "never launder a mid-stream error into an empty success turn" universal, or
  a Claude-specific discipline others lack?
- Do all subjects implement compact-and-retry on overflow, or do some just fail?

## Codex-lens extensions

- **Transport-switch tolerance** — retry exhaustion may trigger a transport
  fallback surfaced as a model-visible warning; cross-ref
  [transport](transport.md).
- **Approval-decision branching** — a denied escalation returns a *structured*
  decision (adapt-and-continue vs abort) the loop must branch on, not a bare
  error; owned by [privilege-approvals](privilege-approvals.md).

## Feeds

`design/bro-harness/bro-harness-api-robustness.md` (the landed Anthropic-transport
robustness work), `design/bro-harness/bro-harness-residuals.md` (R1–R5 residuals).
