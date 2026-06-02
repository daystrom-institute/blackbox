---
title: "Codex · Privilege, Sandboxing & Approvals"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: privilege-approvals
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - privilege-approvals
brief: "Codex: the richest permission model — envelope declared in-prompt (sandbox_mode/network/writable-roots/denied-reads), per-call escalation (sandbox_permissions: require_escalated + model-authored justification + reusable prefix_rule), model-proposed durable policy amendments, reviewer routing (User/auto_review/guardian with risk taxonomy), granular per-category gating, and rich decision cardinality (Approved/ApprovedForSession/Denied/Abort)."
---

# Codex · Privilege, Sandboxing & Approvals

> From the codex-lens discovery mine (general-purpose readers over `~/repos/codex/codex-rs`, 2026-06-02) — the pass that surfaced these axes. **confidence: high** (file:line). Codex's base-axis cells (transport…skills) remain stubs pending a full mining pass.
See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Bidirectional and the most elaborate of the four. **Declaration:** a rendered prompt section states sandbox mode, network access, writable roots, and explicit **denied reads** ("do not request escalation … these are policy restrictions"). **Negotiation:** per-call `sandbox_permissions: require_escalated | with_additional_permissions` + a model-authored `justification` (shown to the human) + a reusable `prefix_rule`; the model may propose **durable execpolicy/network amendments** that persist on approval. **Adjudication:** routing across `User` / `auto_review` sub-agent / **guardian** (risk taxonomy Low→Critical, user_authorization score); **granular** per-category gating (sandbox/rules/skill/mcp_elicitations/request_permissions); decision cardinality `Approved / ApprovedForSession / Denied / Abort`.

**Evidence.**
- `prompts/templates/permissions/approval_policy/on_request.md:28` — "provide the sandbox_permissions parameter with the value require_escalated"
- `core/src/tools/handlers/shell_spec.rs:297` — per-command sandbox override + `prefix_rule`
- `protocol/src/approvals.rs:87` — `GuardianRiskLevel{Low,Medium,High,Critical}`; `protocol.rs:3629` — `ApprovedForSession`

**Vs the axis.** The reference implementation for the whole axis. With claude (`auto` mode) and agy (sandbox prompt), codex is in the **declares-envelope** camp; vibe is the outlier (no declaration).

## Open
<!-- guardian prompt contents; how proposed amendments are persisted server vs local. -->
