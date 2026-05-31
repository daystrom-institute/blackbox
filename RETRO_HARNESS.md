# Retro Harness

This is a retrospective harness for the end of an agent session. Use it to turn the session's tool friction into Blackbox substrate feedback instead of letting it disappear into chat history.

## Prompt

Run this as a non-compelling retrospective: it asks for reflection, not for code changes.

> This is a retrospective. In this session, what tool gaps did you encounter? Which surfaces felt wrong, too narrow, too broad, or missing? What did you reach for that was not there? What manual workaround did you use instead?
>
> File real Blackbox substrate gaps and wishlist items as gap notes. Use the `blackbox.gap_note.v1` JSON envelope in `bbox_note(kind="followup")`, and dedupe first with `bbox_notes(kind="followup", query="blackbox.gap_note.v1", include_addressed=false)`.

## What counts as a gap

File a gap note when the missing capability is in the shared Blackbox substrate or agent workflow and agents in unrelated projects would plausibly hit it too.

Good examples:

- A tool primitive, refactor atom, or MCP surface was missing for a recurring task.
- A surface existed but had the wrong shape for the role: too much authority, not enough authority, poor discoverability, or awkward composition.
- A workflow shape was missing: fork, wait, cancel, resume, review, summarize, or close-out behavior had to be hand-rolled.
- The corpus ontology could not represent a relationship you needed to cite or bundle.
- A runbook, rendered instruction, or system memory was missing for a recurring agent decision.
- An eval or packet surface could not express the case you needed to verify.

Do not file a gap note for ordinary product TODOs, one-off cleanup, or a user-stated standing rule. Standing rules belong in the durable knowledge lanes; task-local state belongs in threads or pins.

## Existing automation

If you are reflecting on a completed dispatched bro task and the `bro_retro` tool is available, prefer that primitive: it resumes the task's own provider session with a non-compelling retrospective prompt and lets the agent self-file gap notes when warranted.

Use this document when you need a portable prompt, a review checklist, or a manual fallback outside that exact `bro_retro(task_id=...)` path.

## Process

1. Review the session and list every moment where you thought, "I wish there were a tool/surface for this."
2. Group similar moments by reusable capability, not by individual annoyance.
3. For each candidate, ask: would this matter in another project or for another agent role?
4. Dedupe against open gap notes before filing.
5. File each real substrate gap with a stable `dedupe_key`.
6. Mention any candidates you intentionally did not file and why.

## Gap-note template

```json
{
  "type": "blackbox.gap_note.v1",
  "title": "Short human-readable gap title",
  "gap_kind": "tooling",
  "domain": "retro-harness",
  "wanted_capability": "Describe the reusable capability the agent wanted.",
  "missing_primitive": "Optional concrete primitive or surface name.",
  "fallback_used": "What the agent did manually instead.",
  "impact": "medium",
  "blocking_level": "workaround_available",
  "evidence": [
    "session retrospective",
    "file:RETRO_HARNESS.md"
  ],
  "dedupe_key": "tooling/retro-harness/capability-slug",
  "suggested_owner": "blackbox",
  "notes": "Any extra context useful for triage."
}
```

Advisory `gap_kind` values include `packet_ast`, `tooling`, `agent`, `workflow`, `refactor_primitive`, `mcp_surface`, `ontology`, `eval_coverage`, and `docs_runbook`.

## Retrospective output shape

When you run this harness, return a short summary:

- Gap notes filed: note ids plus titles.
- Existing notes reused or referenced: note ids plus dedupe keys.
- Wishlist candidates not filed: one line each, with the reason.
- Follow-up risk: anything likely to keep hurting agents if left untriaged.

## Tone

Be concrete and operational. Prefer "I reached for X, found Y, and worked around it by Z" over broad complaints. The goal is not to criticize the current task; it is to preserve reusable substrate feedback for the next iteration of Blackbox.
