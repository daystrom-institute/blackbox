---
title: "Axis: Privilege, Sandboxing & Approvals"
kind: research-axis
corpus: blackbox-research
track: harness
axis: privilege-approvals
topic:
  - harness
  - privilege-approvals
  - governance
brief: "Cross-harness invariant model for the privilege axis: the bidirectional, agent-facing permission model. Harness→model: declaration of the operating envelope (sandbox mode, network access, writable roots, denied reads), injected and updated differentially. Model→harness: per-call escalation requests with model-authored justification and reusable approval rules, and model-proposed durable policy amendments. Decision/reviewer side: approval routing (human / auto-review / LLM-guardian), granular per-category gating, and a decision cardinality the model branches on. Surfaced by the codex-lens discovery pass as the single biggest gap in the first-pass taxonomy."
---

# Axis: Privilege, Sandboxing & Approvals

> **Scope.** The agent-facing *permission model* — what the model is allowed to
> do, how it learns its envelope, how it negotiates more, and who adjudicates.
> This is distinct from [hooks](hooks.md) (a deterministic operator seam) and
> from OS-level sandboxing internals (not agent-facing). Only the parts the model
> *perceives or acts on* belong here.
>
> **Surfaced by:** the codex-lens bottom-up pass (multi-agent convergence). The
> first-pass top-down taxonomy folded privilege into hooks and missed it as a
> first-class, *bidirectional* dimension.

## The dimension

A harness that lets an agent run autonomously must tell it what it may do, and
must give it a channel to ask for more — safely. This is the governance core of
agentic autonomy, and it is fundamentally **bidirectional**:

- **Declaration (harness→model):** the model is told its operating envelope so
  it doesn't waste turns attempting forbidden actions or, worse, assume it can't
  do something it can.
- **Negotiation (model→harness):** the model requests escalation, authors the
  justification a human/reviewer sees, and may propose durable rule changes.
- **Adjudication (reviewer→model):** a decision is returned with enough
  structure that the model knows whether to adapt, retry, or halt.

For a harness whose goal is "steer agents to tooling without bloat," this axis is
where *safe* autonomy is won or lost: get it wrong and you either over-prompt
(bloat) or under-inform (the agent flails or acts unsafely).

## Questions a finding must answer

- **Envelope declaration.** Is sandbox mode (read-only / workspace-write /
  full), network access, writable roots, and explicit **denied reads** injected
  into the model's context? As a coherent block or scattered fragments?
- **Differential updates.** When the envelope changes mid-session (a prefix
  approved, a network rule added), is a *delta* injected, or is the whole policy
  re-sent every turn (bloat)?
- **Per-call escalation protocol.** Can the model request elevated privilege per
  tool call (e.g. an escalation flag + a model-authored `justification` the user
  reads + a reusable approval `prefix_rule`)? Is this in the tool schema?
- **Model-proposed policy amendments.** Can the model propose durable rule
  changes (allow this prefix, permit this host) that persist beyond the turn?
- **Approval routing / reviewer identity.** Who adjudicates — human, an
  auto-review sub-agent, an LLM **guardian** with a risk taxonomy? Does the model
  know which (it shapes how it justifies)?
- **Granularity.** Are approval channels independently gated (exec / rules / MCP
  elicitations / skill scripts), with the model told which will prompt vs be
  auto-rejected?
- **Decision cardinality.** Is the outcome a boolean, or a richer set
  (Approved / ApprovedForSession / Denied=adapt-and-continue / Abort=halt) the
  model branches on?

## Convergence / divergence

| Subject | Envelope declared to model? | Per-call escalation | Reviewer | Decision cardinality | Cell |
|---|---|---|---|---|---|
| Claude | **yes** — 5 modes + allow/deny rules DSL | `acceptEdits`/`auto`; hook `permissionDecision` | human / `auto` self-assess / PreToolUse hook | allow/deny/ask | [claude](claude/claude-privilege-approvals.md) |
| Codex | **yes** — sandbox+net+writable+denied-reads | `require_escalated` + justification + prefix_rule (+ policy amendments) | User / auto_review / guardian (Low–Critical) | Approved/ForSession/Denied/Abort | [codex](codex/codex-privilege-approvals.md) |
| Antigravity | **yes** — `run_command` sandbox prompt | `BypassSandbox:true` ⇒ user approval | user (`proceed-in-sandbox` auto) | approve/deny | [antigravity](antigravity/antigravity-privilege-approvals.md) |
| Vibe | **no** — enforced externally | profile-gated (no model channel) | human TUI (yes/no/always) | yes/no/always(+persist) | [vibe](vibe/vibe-privilege-approvals.md) |

**Synthesis (4 subjects).** The headline divergence is **envelope declaration**: codex, claude, and agy all *tell the model* its sandbox/permission posture; **vibe deliberately does not** (constraints enforced externally via middleware + tool-skip feedback) — a clean 3-vs-1 design fork. Per-call escalation with a **model-authored justification** + reusable rule is codex-distinctive (claude's `auto` self-assessment is the nearest analogue). An **LLM reviewer** (guardian/auto_review) is codex-only.

## Open invariants

<!-- TODO(synthesis): -->
- Is envelope-declaration universal, or do some harnesses leave the model to
  discover limits by hitting them?
- Is model-authored justification (the model writes the approval question the
  human reads) a convergent idiom or codex-specific?
- Where does the LLM-guardian (model-as-gate) pattern recur?

## Feeds

`design/bro-harness/backlog-per-call-escalation.md` (Codex-style escalate +
justification, gated on unifying the privilege model). bro-harness privilege
lives in `SafetyPolicy` + the brofile allow/deny layer — this axis is the
reference for making that bidirectional and model-facing.
