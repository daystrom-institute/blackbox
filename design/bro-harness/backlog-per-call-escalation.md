---
title: "bro-harness per-call privilege escalation (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The Codex-style per-call escalation model for shell_run and future networked tools: least-privilege by default, require escalate:true + justification for mutating/networked commands, recorded for audit. Deferred because it overlaps the brofile allow/deny layer and the static SafetyPolicy; shipping it now would create two competing privilege systems. Gated on unifying the privilege model first."
---

# bro-harness per-call privilege escalation (backlog)

> **Provenance.** Extracted from [`bro-harness-tool-surface.md`](./bro-harness-tool-surface.md)
> ("Per-call escalation — deferred, spec only").

## Status / gate

**Not built in v1, intentionally.** It overlaps the brofile allow/deny layer and
the static `SafetyPolicy`; shipping it now would create two competing privilege
systems. **Gate:** revisit only once the privilege model is unified. The
`shell_run` shape can gain the two fields without breaking callers, so deferral
costs nothing in forward-compatibility.

## The shape (spec)

The Codex model: run `shell_run` (and any future networked tool) least-privilege
by default; require `escalate: true` + `justification` for mutating or networked
commands, recorded for audit.

## Approach (when the gate opens)

- Unify the privilege model first: decide the single authority among
  `SafetyPolicy`, the brofile allow/deny layer, and any per-call escalation, so
  there is one place a command's privilege is decided.
- Add `escalate` + `justification` to the `shell_run` arg shape (additive, no
  break).
- Record every escalation for audit, consistent with the existing audit trail.

## Acceptance

- A mutating/networked command without `escalate: true` is refused by the unified
  privilege authority, not by two systems disagreeing.
- Escalations are audit-logged with their justification.
- No second privilege system: `SafetyPolicy`/brofile allow-deny and per-call
  escalation compose under one decision point.

## Relationship

- Parent: [`bro-harness-tool-surface.md`](./bro-harness-tool-surface.md).
- Privilege also lives in `SafetyPolicy` + the brofile allow/deny layer.
- Cluster map: [`bro-harness.md`](./bro-harness.md).
