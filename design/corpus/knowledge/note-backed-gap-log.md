---
title: "Note-backed gap log"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
  - knowledge
superseded_by: "first-class repo-owned gap store (src/gaps.rs, bbox_gap* tools, committed .bbox/gaps/); see sm-gap-notes"
---

# Note-backed gap log

Status: implemented and archived after `32d15e7`
Date: 2026-05-12
Related: `src/notes.rs`, `src/inbox.rs`, `src/tools/packets.rs`,
`src/system_memory/gap-notes.md`, `design/corpus/knowledge/note-backed-gap-log-impl.md`

## Thesis

Blackbox already has the right primitive for cross-agent operational signals:
`bbox_note`. The missing piece is a uniform convention for using notes to
report substrate gaps from any project on the machine.

A gap is not a new store in v1. It is an unresolved note with a structured
body that says: "while doing real work, an agent needed a tooling, agent,
workflow, ontology, packet-AST, or refactor primitive that does not exist yet."

The packet gap log proved the shape. This proposal generalizes that behavior
without introducing a separate `bbox_gap` API. If future pressure shows that
notes are too weak, that can be proposed later from actual gap-note traffic.

## Goals

- Give agents in any project a clear way to report missing blackbox substrate
  capability without asking the operator where to put it.
- Keep the reporting surface small: `bbox_note`, `bbox_notes`,
  `bbox_note_resolve`, and `bbox_inbox`.
- Preserve workstream provenance: task, session, provider, bro, thread, and
  project should remain attached to the report.
- Make gap reports queryable and triageable through the existing inbox.
- Support agents that do not have MCP by providing a file spool format that
  can be ingested into notes.
- Close the loop when the supporting implementation lands, ideally in the same
  commit that addresses the gap.

Non-goals:

- A new `bbox_gap_*` tool family.
- A new durable policy-memory lane.
- Replacing roadmap items or work-item threads.
- Turning every deferred task into a gap.

## What Counts As A Gap

Use a gap note when the blocker is in the blackbox substrate or shared agent
workflow, not merely in the current product codebase.

Good examples:

- "I needed packet AST support for temporal/rate predicates and fell back to
  prose review."
- "I needed a Java refactor atom for safe enum extraction and had to hand-edit."
- "The workflow engine cannot express this wait/cancel shape."
- "The MCP surface for outside agents exposes too much or too little for this
  recurring role."
- "The ontology has no place to represent this reusable relationship, so
  evidence cannot be bundled cleanly."

Not gap notes:

- Ordinary TODOs in the current repo.
- Missing application features.
- One-off cleanup left after a task.
- User-stated standing rules; those belong in knowledge.
- Active-arc instructions; those belong in pins or threads.

The test is: would agents in other projects plausibly hit the same missing
blackbox capability? If yes, report a gap. If no, use a normal followup note.

## Canonical Note Shape

V1 uses `bbox_note(kind="followup")` with a structured JSON body. The body is
JSON so the existing note store can carry it without schema changes.

This deliberately overloads `followup` in phase 1. Until the dedicated inbox
section lands, gap notes will also appear in the ordinary followup stream. That
is acceptable for a small convention-only rollout, but broad deployment should
ship the inbox grouping early so substrate gaps do not drown task-local
followups.

Recommended body:

```json
{
  "type": "blackbox.gap_note.v1",
  "title": "Packet AST cannot express rate predicates",
  "gap_kind": "packet_ast",
  "domain": "review-policy",
  "wanted_capability": "Classify entities by count/rate within a time window.",
  "missing_primitive": "RateCmp / WithinWindow",
  "fallback_used": "Prose rubric plus manual review.",
  "impact": "medium",
  "blocking_level": "workaround_available",
  "evidence": [
    "packet-event:gap:2026-05-12T19:21:45Z",
    "thread-7f01324e"
  ],
  "dedupe_key": "packet_ast/rate-window-predicate",
  "suggested_owner": "blackbox",
  "notes": "Observed while trying to compile a reusable review packet."
}
```

Suggested `gap_kind` values are advisory, not a closed enum:

- `packet_ast`
- `tooling`
- `agent`
- `workflow`
- `refactor_primitive`
- `mcp_surface`
- `ontology`
- `eval_coverage`
- `docs_runbook`

Suggested `impact` values are advisory:

- `low` - nuisance, easy manual workaround
- `medium` - repeated friction or weak mechanization
- `high` - blocks useful automation or causes recurring bad agent behavior
- `critical` - causes unsafe edits, data loss risk, or unusable workflows

Suggested `blocking_level` values are advisory:

- `none`
- `workaround_available`
- `blocks_task`
- `blocks_class_of_work`

The note's normal fields still matter:

- `project` records where the gap surfaced.
- `thread_id` links to an active work item when available.
- `task_id`, `session_id`, `provider`, and `bro` preserve authoring context.
- `resolution` is the lifecycle state.

## Lifecycle

Use existing note resolution states as the lifecycle:

- `unresolved` - reported and not triaged.
- `acknowledged` - seen, deduped, or accepted for later handling.
- `addressed` - implemented, rejected, superseded, or intentionally closed.

Resolution notes carry the terminal reason:

```text
implemented in commit abc123; added packet predicate WithinWindow
duplicate of note-a1b2c3d4
rejected: application-specific TODO, not blackbox substrate
superseded by roadmap item BB-RX-14
```

If a gap becomes real implementation work, open or link a normal
`bbox_thread(kind="work_item")` or roadmap item. The note remains the original
field report; the thread or roadmap item becomes the execution object.

## Inbox Behavior

`bbox_inbox` should gain a "Gap notes" section by scanning unresolved or
acknowledged notes whose body parses as JSON with
`type == "blackbox.gap_note.v1"`. If JSON parsing fails, it may fall back to a
substring check for `"blackbox.gap_note.v1"` so hand-authored bodies degrade
gracefully.

The inbox row should show:

- note id
- `gap_kind`
- `impact`
- project leaf
- title
- dedupe key when present

Ordering should prioritize:

1. unresolved before acknowledged
2. higher impact before lower impact
3. newer before older

This keeps gaps visible without making every followup note look like substrate
work.

## File Spool Fallback

Agents without MCP can write the same JSON envelope to a spool file.

Preferred local path:

```text
<project>/.bbox/gaps/inbox/<timestamp>-<slug>.json
```

Host-wide fallback:

```text
~/.local/share/blackbox/gaps/inbox/<timestamp>-<slug>.json
```

The file content is the same `blackbox.gap_note.v1` object. A lightweight cron
or daemon sweep can import valid files by creating `bbox_note(kind="followup")`
records, then move imported files to:

```text
<project>/.bbox/gaps/imported/
~/.local/share/blackbox/gaps/imported/
```

For versioned project-local gap files, the commit that implements the missing
capability may remove or move the corresponding file. The canonical close-out
is still the note resolution, because notes retain cross-project provenance and
remain queryable after the file is gone.

## MCP Surface

No new MCP tools are required. A narrow optional surface can make reporting
discoverable for outside agents:

```text
surface = gap-reporting
allow = [
  "bbox_note",
  "bbox_notes",
  "bbox_note_resolve",
  "bbox_inbox"
]
deny = [
  "bro_*",
  "bbox_learn",
  "bbox_remember",
  "bbox_decide",
  "bbox_forget",
  "bbox_render"
]
```

The surface prompt should teach one operation:

1. emit `bbox_note(kind="followup")`
2. put the `blackbox.gap_note.v1` JSON object in `body`
3. include `project`, `task_id`, `session_id`, `provider`, `bro`, and
   `thread_id` when available

This gives other projects a stable reporting path without expanding the core
tool ontology.

## Dedupe And Triage

Before filing a gap, agents should search recent open gap notes:

```text
bbox_notes(kind="followup", query="blackbox.gap_note.v1", include_addressed=false)
```

If the agent finds a likely duplicate, it should add a normal followup note
that references the existing note id, or report the new occurrence with the
same `dedupe_key`. Triage can then acknowledge or address duplicates with a
resolution note.

`dedupe_key` should be stable and boring:

```text
<gap_kind>/<domain>/<missing-primitive-or-capability>
```

Examples:

```text
packet_ast/review-policy/rate-window-predicate
workflow/webhook-routing/cancel-by-correlation
refactor_primitive/java/extract-enum
```

Packet-specific AST gaps may still use `bbox_packet_gap` because that tool
captures packet authoring fields and writes to the packet event log. That log is
not the note store, so packet gaps are not automatically visible to
`bbox_notes` or `bbox_inbox`. The convention is:

- use `bbox_packet_gap` when actively authoring a packet and the packet AST is
  the missing surface
- use a `blackbox.gap_note.v1` note for all non-packet substrate gaps
- when packet gaps need unified inbox visibility, teach `bbox_packet_gap` to
  emit a companion gap note internally instead of asking callers to double-file

## Close-out Discipline

The implementation commit that fills a gap should close the loop:

- update or add the supporting code/tests/docs
- resolve the note as `addressed`
- include the note id in the resolution text
- remove or move any project-local spool file if one exists
- optionally mention the note in the commit body:

```text
Addresses-Gap-Note: note-a1b2c3d4
```

Do not delete the canonical note record. Addressed notes are hidden from normal
`bbox_notes` and `bbox_inbox` views unless explicitly requested, but they remain
available for provenance and repeated-gap analysis.

## Implementation Sketch

Phase 1: conventions only.

- Document the JSON envelope.
- Teach agent instructions to emit this note shape when substrate gaps appear.
- Manually query with `bbox_notes(kind="followup", query="blackbox.gap_note.v1")`.

Phase 2: inbox support.

- Add a "Gap notes" section to `bbox_inbox`.
- Parse JSON bodies opportunistically; malformed bodies remain ordinary notes.
- Add tests for ordering and project filtering.

Phase 3: spool importer.

- Discover `.bbox/gaps/inbox/*.json` under registered projects and the
  host-wide spool.
- Validate the envelope.
- Create a followup note.
- Move imported files to `imported/`.
- Report invalid files as `surprise` notes or importer diagnostics.

Phase 4: close-out helpers.

- Add a linter or cron check for stale high-impact gap notes.
- Optionally scan commits for `Addresses-Gap-Note: note-...` and verify the
  note is addressed.
- Produce a periodic summary grouped by `gap_kind`, `domain`, and
  `dedupe_key`.

## Open Questions

- Should `NoteParams` grow a generic `metadata: serde_json::Value` field, or is
  JSON-in-body good enough until traffic proves otherwise?
- Should `bbox_packet_gap` emit companion gap notes immediately when inbox
  grouping lands, or only after packet gap volume proves that unified visibility
  matters?
- Should the file spool importer be daemon startup work, a cron, or a manual
  admin command?
- Should accepted gaps always become roadmap items, or only when they need
  multi-session implementation?
