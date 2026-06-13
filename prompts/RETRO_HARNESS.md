---
title: "Retro Harness"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
brief: "End-of-session self-report for a bro fleet / bro-harness session: what felt helpful, noisy, missing, or awkward."
---

# Retro Harness

Use this when an operator wants the agent to report how the **harness itself**
felt at the end of a real session. The goal is not task closeout and not a
generic product TODO list. The goal is to preserve concrete feedback about the
agent loop, injected context, tools, sandbox, steering, and orchestration
surfaces while the experience is still fresh.

This is a hand-driven prompt. A human points a live agent at it after a session
with meaningful tool use, steering, dispatch, or harness interaction. The agent
should answer from first-hand experience: what helped, what was noisy, where it
lost time, which tool it reached for but did not have, and which friction is
worth turning into a reusable substrate gap.

## Scope

Reflect on the harness and sandbox, not the product feature you were building.

Include these surfaces when they were relevant:

- **Prompt and injected context**: developer directives, AGENTS/PROJECT context,
  `bbox_scope`, dispatch context, reminders, hook riders, system-tail messages,
  and any context that arrived at the wrong time or repeated too often.
- **Tool surface**: built-in tools, MCP tools, code/search/read/edit helpers,
  shell behavior, async command/session behavior, todo/checklist behavior,
  report/progress tools, and whether the available surface matched the move you
  wanted to make.
- **Nudger and ledger behavior**: whether hook nudges were timely, throttled,
  actionable, too forceful, or stale; whether repeated suggestions felt like
  useful guardrails or conversation noise.
- **Sandbox and observability**: cwd/worktree identity, writable roots, dirty
  tree visibility, env/provider/session details, tool-denial clarity, and
  whether an outside operator could reconstruct what happened.
- **Blackbox grounding path**: whether recall, graph search, gap store, notes,
  threads, evidence bundles, and provenance tools were easy to choose correctly
  for the task shape.
- **Fleet / steering / resume loop**: composer behavior, queued input, harness
  echo, interrupt/resume semantics, activity/status display, and whether the
  session recovered cleanly after steering or compaction.
- **Missing or awkward primitives**: tools you reached for that were unavailable,
  wrong-shaped, too verbose, too hidden, or hard to compose.
- **Intern / classifier / advisor side-channel**: whether it helped, distracted,
  duplicated visible state, or needed a different role/format.

## Operator Prompt

Point the agent at this file and ask it to run the retro. The intended answer is
a concise but specific self-report.

> Run a harness retro for the session we just had. Focus on how the harness and
> sandbox felt, not on the product code. What was helpful? What was noisy? Where
> did the harness steer you wrong or repeat itself? Which tools did you reach for
> that were missing, unavailable, or awkward? Where did you lose time because a
> surface was too verbose, too quiet, stale, or hard to compose? What did the
> nudger/ledger get right or wrong? Did sandbox/status/progress reporting make
> the session observable enough? Which issues should become durable substrate
> gaps, and which are just notes or wishlist items?

## How To Answer

Return the retro in this shape:

1. **Overall feel**: one short paragraph with the dominant impression.
2. **Helpful**: bullets for things that reduced friction or improved safety.
3. **Noisy / awkward**: bullets for things that distracted, over-fired,
   repeated, produced too much output, or forced manual workaround.
4. **Missing / wishlist**: bullets for primitives, views, or workflows you
   reached for but did not have.
5. **Evidence from the session**: concrete moments, tools, files, commands, or
   UI states that support the assessment.
6. **Gaps**: existing gap ids referenced, new gaps filed, or "none filed" with
   the reason.
7. **One next harness improvement**: the single change most likely to improve the
   next session like this.

Keep it operational: "I reached for X, got Y, worked around it with Z" is better
than broad commentary.

## Gap Filing

File a gap only when the issue is reusable harness/substrate friction that
another agent could plausibly hit. Dedupe first with `bbox_gaps` by
`dedupe_key`, `gap_kind`, or domain. Use `bbox_gap` with typed params when filing.

Good gap candidates:

- A nudge fired repeatedly or carried stale guidance after the system was fixed.
- A tool was unavailable, hidden behind the wrong surface, or had an output shape
  that pushed the agent into noisy workarounds.
- The sandbox boundary, dirty-tree state, install target, or service/process
  ownership was unclear.
- The steering/resume/queued-input loop made state ambiguous or durable scrollback
  misleading.
- Evidence/provenance tooling made the correct path hard to choose.
- A recurring workflow needed a first-class primitive instead of manual ceremony.

Do not file a gap for:

- Ordinary feature work, cleanup, or test debt in the product.
- A user-stated standing rule; that belongs in durable knowledge if the operator
  approves it.
- A one-off annoyance that is unlikely to generalize.
- Something already fixed in the same session, unless the point is to record a
  recurrence or validation gap that still exists.

## Gap Call Template

```text
bbox_gap(
  title="Short human-readable gap title",
  gap_kind="tooling|mcp_surface|docs_runbook|workflow|agent|ontology|refactor_primitive|eval_coverage|packet_ast",
  domain="harness/<area>",
  wanted_capability="Reusable harness/substrate capability wanted.",
  missing_primitive="Concrete missing or wrong-shaped primitive, if known.",
  fallback_used="What the agent did instead.",
  evidence=["task:<id>", "session:<id>", "tool:<name>", "file:<path>"],
  impact="low|medium|high",
  blocking_level="none|workaround_available|blocking",
  dedupe_key="<kind>/<domain>/<stable-slug>",
)
```

If you file nothing, say why. A good retro can be pure self-report when the
session already fixed the main harness defects.
