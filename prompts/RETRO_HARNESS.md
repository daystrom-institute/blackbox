---
title: "Retro Harness"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
brief: "End-of-session retro for a bro fleet / bro-harness session (built-in tools, injected context, intern, turn machinery). Files substrate gaps."
---

# Retro Harness

A retrospective pass for an agent session, focused on **the harness and sandbox the agent ran inside** — the built-in tools, sandbox/worktree boundary, injected context, side-channel intern, and turn machinery of `bro-harness`. Use it to turn harness friction into reusable Blackbox substrate feedback instead of letting it dissolve into chat history.

This is a manual, human-steered pass. When you finish a session in `bro fleet` where real tool-calling happened and the harness behavior is worth reflecting on, point the agent at this document and have it file gap notes. It is deliberately not wired into the dispatch pipeline — you invoke it with judgment, on the sessions that warrant it. (`bro_retro` is the separate automated path for completed dispatched tasks; this doc is the hand-driven counterpart for live TUI sessions.)

## What this reflects on

The subject is the harness, not the broader Blackbox daemon. Concretely:

- **Built-in tools** — the harness's own tool surface: `shell_run` (in a fleet session it starts a harness-local promise — the blocking yield/`shell_poll` path is deliberately unavailable here), the promise lifecycle tools (`promise_status`/`promise_wait`/`promise_when_all`/`promise_when_any`/`promise_cancel`), file read/edit/write, NARF/KV state, the todo list, `web_search`, `report`. Was any of them too narrow, too broad, awkward to compose, or simply missing?
- **Injected context** — the system-prompt overlay (AGENTS.md discovery), hook nudges (riders and system-tail messages), tail nudges, and tool-result bounding/spill. Did an injection fire at the wrong time, say the wrong thing, crowd the window, or fail to surface when it should have?
- **Sandbox grounding and observability** — the cwd/worktree boundary, writable roots, provider/session env, MCP surface, file/shell tool capabilities, and any sandbox manifest or grounding text. Did you know where you were allowed to read/write? Could you tell whether you were in a managed worktree, base checkout, or host home? Were denials, cwd changes, env overrides, and file writes visible enough afterward for another agent/operator to debug?
- **Blackbox evidence grounding** — the agentic opening sequence and evidence bundle path: `bbox_describe_schema`, `bbox_hybrid_search`, `bbox_inspect_entity`, conditional `bbox_find_paths`, and `bbox_bundle_evidence`. When a claim depended on design docs, prior decisions, threads, code graph facts, or history, was the path clear enough to bundle evidence before answering?
- **Sandbox-native idioms** — shorthand or native tool shapes such as `note()`, `hybrid_search()`, `smart_read()`, or `work_bash()` versus fully-qualified MCP names. Did the sandbox give you ergonomic primitives for the thing you needed, or did you have to translate through old outside-daemon conventions?
- **The intern** — the side-channel classifier/advisor companion, when one was active. Did it help (caught an error, flagged a gap, suggested a better path) or was it noise (distracting, ill-timed, wrong model for the role, awkward format)?
- **Turn machinery** — steering, interrupt, replay, compaction (automatic and `/compact`), and the MCP tool admission/deny surface. Did the loop behave when you steered or interrupted? Did compaction drop something it shouldn't have? Was the admitted tool set the right one?
- **The promise model** — in a fleet session every `shell_run` is a harness-local promise: it starts immediately and its completion auto-injects a hidden `HARNESS_EVENT` wake turn instead of making you poll. Reflect on that shape: did the auto-wake fire at a good boundary, or cut in at a bad one? Did `promise_wait`/`promise_when_all`/`promise_when_any`/`promise_cancel` compose for the work you had? Did running-progress metadata (elapsed, last-output, byte counts) give enough signal that a quiet command was still healthy, or did you feel blind between start and wake?

## The prompt

Run this as a non-compelling retrospective — it asks for reflection on the harness, not for code changes.

> This is a retrospective on the harness and sandbox you just ran inside — its built-in tools, sandbox/worktree boundary, injected context, blackbox evidence grounding path, intern (if one was active), and turn machinery. Where did the harness get in your way? What built-in tool was missing, too narrow, or awkward to compose? Did the sandbox grounding tell you the cwd, writable roots, durable project scope, provider/session env, and MCP surface clearly enough? If the task depended on design docs, prior decisions, threads, code graph facts, or history, was the agentic opening sequence and `bbox_bundle_evidence` path clear enough? Could an outside observer reconstruct what files, commands, env overrides, denials, evidence refs/bundles, and tool calls mattered? Did an injected nudge or system message fire at the wrong moment or say the wrong thing? If an intern was advising you, did it help or was it noise? Did steering, interrupt, or compaction misbehave? Every `shell_run` here is a promise that wakes you on completion instead of making you poll — did that auto-wake model fit the work, or did you fight it? What did you reach for that the harness or sandbox did not give you, and what did you do instead?
>
> File real harness or substrate gaps with `bbox_gap` (typed params, not a note envelope), and dedupe first with `bbox_gaps` (filter by `dedupe_key` / `gap_kind` / `domain`).

## What counts as a gap

File a gap note when the missing or wrong-shaped capability is in the harness (or the shared substrate it exposes) and an agent in another session or project would plausibly hit it too.

Good examples:

- A built-in tool was missing for a recurring move, or had the wrong shape — too much output with no way to scope it, awkward to chain into the next call.
- The sandbox boundary was unclear or invisible: no first-class manifest for cwd/base/worktree, writable roots, env overrides, denied paths, MCP surface, or active provider/account/session.
- The blackbox evidence path was unclear or awkward: an agent could not easily retrieve, inspect, traverse, or bundle evidence before making a provenance-sensitive claim.
- A sandbox-native primitive was missing or awkward, forcing old outside-daemon forms where a direct in-sandbox idiom would have been clearer.
- An injected nudge or system-tail message was mistimed, redundant, or wrong, and steered the agent off course.
- Compaction dropped context that was still needed, or fired too late to help.
- The intern's signal-to-noise was poor: wrong model for the role, interjections that broke flow, an awkward side-channel format.
- The MCP admission surface hid a tool that was needed, or admitted noise the agent had to wade through.
- Steering or interrupt did not behave as expected at a turn boundary.

Do not file a gap note for ordinary product TODOs, one-off cleanup, or a user-stated standing rule. Standing rules belong in the durable knowledge lanes; task-local state belongs in threads or pins. Feedback that is purely about one intern instance (rather than a reusable capability) belongs in the summary, not a note.

## Process

1. Start with a boundary inventory: cwd, managed worktree/base checkout, writable roots, provider/session env, MCP surface, evidence-bundle path, and what evidence made each clear.
2. Walk the session and list every moment of "I wish the harness did X" or "why did the harness do Y."
3. Group by reusable capability, not by individual annoyance.
4. For each candidate, ask: would this bite another agent, in another session or project?
5. Dedupe against open gaps (`bbox_gaps`) before filing.
6. File each real gap with a stable `dedupe_key`.
7. Note any candidates you deliberately did not file, and why.

## Gap-filing call

```text
bbox_gap(
  title="Short human-readable gap title",
  gap_kind="tooling",
  domain="harness/shell-tools",
  wanted_capability="Describe the reusable harness/substrate capability the agent wanted.",
  dedupe_key="tooling/harness/capability-slug",
  impact="medium",
  blocking_level="workaround_available",
  missing_primitive="Optional concrete tool, injection, or surface name.",
  fallback_used="What the agent did manually instead.",
  evidence=["session retrospective", "file:RETRO_HARNESS.md"],
)
```

Required: `title`, `gap_kind`, `domain`, `wanted_capability`, `dedupe_key`. See `sm-gap-notes` via `bbox_knowledge` for the full runbook.

`gap_kind` names the *type of capability* that is missing, not the subsystem it lives in. Use one of the existing values — `tooling`, `agent`, `workflow`, `mcp_surface`, `docs_runbook`, `ontology`, `refactor_primitive`, `eval_coverage`, `packet_ast` — and do not coin new ones. There is deliberately **no `harness` kind**: the harness is a *domain*, not a capability type, so it goes in the `domain` field (`harness`, or a scoped `harness/<area>` such as `harness/shell-tools`, `harness/intern`, `harness/compaction`) while the kind stays the capability type. For a harness retro the common kinds are `tooling` (built-in tools), `agent` (the intern, or the harness's own loop behavior), `mcp_surface` (the admitted tool set), and `workflow` (steering / interrupt / compaction shape). Likewise, an aspirational "would be nice" item is not its own kind — file it under the matching capability kind with `blocking_level: "none"` and let `fallback_used` say what you did instead.

## Retrospective output shape

When you run this harness, return a short summary:

- Gaps filed: gap ids plus titles.
- Existing gaps reused or referenced: gap ids plus dedupe keys.
- Sandbox assessment: clear / mixed / unclear, with the most important missing observation surface.
- Intern assessment, when one was active: helped / noise / mixed, in one sentence.
- Wishlist candidates not filed: one line each, with the reason.
- Follow-up risk: anything likely to keep hurting agents if left untriaged.

## Tone

Be concrete and operational. Prefer "I reached for X, the harness gave me Y, I worked around it with Z" over broad complaints. The goal is not to criticize the task; it is to preserve reusable harness feedback for the next iteration of Blackbox.
