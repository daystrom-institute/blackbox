---
title: "Antigravity · Planning & Goal State"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: planning-goals
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - planning-goals
brief: "Planning appears artifact/session-scoped, not a durable goal contract. CLI strings indicate planning-mode artifacts and task/walkthrough/implementation-plan flow; SDK exposes triggers and finish schemas but no first-class durable goal API beyond Conversation state and user-defined tools/hooks."
---

# Antigravity · Planning & Goal State

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Planning & Goal State](../planning-goals.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Planning mode runs a gated lifecycle: **Research** (no source edits) → **implementation_plan.md** (goal, open questions, proposed changes w/ diffs/mermaid) → **STOP for user approval** → **Execute** (work a `task.md` checklist, `[ ]`/`[/]`/`[x]`) → **Verify** → **walkthrough.md** summary. Artifacts carry versioned metadata (`artifactType`, `summary`, `updatedAt`, `version`, `requestFeedback`) and live under the brain dir. `PLAN_STATUS_PLANNING` state; `request_artifact_feedback_stop`. Goals appear session-scoped via `task.md`; no durable multi-session goal contract found.

**Evidence.**
- binary plan template (lines ~175195): Research→Plan→Approve→Execute→Verify; artifacts task.md/implementation_plan.md/walkthrough.md
- `target_architecture_blueprint.md.metadata.json`: `{"artifactType":"ARTIFACT_TYPE_IMPLEMENTATION_PLAN","version":"3","requestFeedback":true}`

**Vs the axis.** Strongly confirms the per-turn-plan facet — agy's plan is the richest (3 structured, versioned, review-gated artifacts). **Divergence:** unlike codex (budgeted goal) / claude (condition `activeGoal`), no durable goal — agy's planning is artifact-driven and session-scoped.

## SDK/local harness update (2026-06-02)

The SDK does not expose a durable goal primitive comparable to Blackbox goals. It has Conversation state, response_schema, a configurable finish tool schema, hooks, and triggers. Triggers can push messages into an agent periodically or on file changes, so background task initiation is possible, but task identity and completion semantics are left to user code or the model/tool contract.

The standalone CLI still has stronger planning signals. Binary prompt-template paths include planning mode and planning-mode artifacts, and earlier strings indicated implementation_plan, task, and walkthrough artifacts with approval/feedback semantics. Those are best understood as workflow artifacts inside a session or brain context, not as a proven cross-session goal ledger.

For Blackbox design, Antigravity is useful mostly as a counterexample: rich planning UI/artifacts can exist without a typed durable goal state machine in the public SDK.

## Open
<!-- Whether plan artifacts re-load across sessions (brain persists them on disk). -->
