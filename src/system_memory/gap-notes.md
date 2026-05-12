# Gap notes — report substrate gaps via `bbox_note`

When the blocker is in the blackbox substrate or shared agent workflow — not in the current product codebase — file a gap note.

There is no `bbox_gap` tool. Do not invent one. The shape below rides inside the existing `bbox_note(kind="followup")` surface so every agent on the host can report a gap without a new MCP catalog entry.

## When to file

Use a gap note when the missing capability is plausibly hit by agents in other projects too.

Good triggers:

- packet AST cannot express the predicate you needed
- a refactor primitive does not exist for the language at hand
- the workflow engine cannot express a wait / cancel / fork shape
- the MCP surface for outside agents is too narrow or too wide for a recurring role
- the ontology has no edge or entity for the relationship you wanted to bundle
- a runbook or rendered memory is missing for a recurring agent decision

Not a gap note:

- ordinary TODOs in the current repo
- missing product features
- one-off cleanup left after the task
- user-stated standing rules (those go to `bbox_learn` / `bbox_decide`)
- active-arc instructions (those go to `bbox_pin` or a work-item thread)

The test: would agents in unrelated projects plausibly hit the same missing blackbox capability? If yes, gap note. If no, normal `followup` note.

## How to file

1. Call `bbox_note(kind="followup")`.
2. Put a `blackbox.gap_note.v1` JSON object in `body`.
3. Set `project`, `task_id`, `session_id`, `provider`, `bro`, and `thread_id` whenever the ambient `[scope]` block has them. These preserve cross-project provenance.

## Envelope

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
  "dedupe_key": "packet_ast/review-policy/rate-window-predicate",
  "suggested_owner": "blackbox",
  "notes": "Observed while compiling a reusable review packet."
}
```

`type` is the routing tag — exact string, no synonyms.

## Advisory vocabularies

`gap_kind`:

- `packet_ast` — predicate the rule-packet AST cannot express
- `tooling` — missing CLI / shell / refactor helper
- `agent` — needed dispatchable agent that does not exist
- `workflow` — missing arc / wait / fork / cancel shape
- `refactor_primitive` — language-specific refactor atom
- `mcp_surface` — wrong allow/deny shape for a recurring role
- `ontology` — missing entity type or edge family
- `eval_coverage` — packet or test eval cannot reach a class of cases
- `docs_runbook` — missing rendered guidance or runbook

`impact`:

- `low` — nuisance, easy manual workaround
- `medium` — repeated friction or weak mechanization
- `high` — blocks useful automation or causes recurring bad agent behavior
- `critical` — causes unsafe edits, data-loss risk, or unusable workflows

Default to `medium` when unsure.

`blocking_level`:

- `none`
- `workaround_available`
- `blocks_task`
- `blocks_class_of_work`

`dedupe_key`:

```text
<gap_kind>/<domain>/<missing-primitive-or-capability-slug>
```

Stable and boring. Examples:

```text
packet_ast/review-policy/rate-window-predicate
workflow/webhook-routing/cancel-by-correlation
refactor_primitive/java/extract-enum
```

## Before filing — dedupe

Search recent open gap notes first:

```text
bbox_notes(kind="followup", query="blackbox.gap_note.v1", include_addressed=false)
```

If a match is open, do not file a new note. Either add a normal `followup` referencing the existing id, or file a new occurrence with the same `dedupe_key` so the operator can tally recurrences. Addressed notes are NOT a dedupe hit — a recurrence after close-out is itself signal.

## Packet AST gaps

If you are actively authoring a packet and the AST is the missing surface, use `bbox_packet_gap` directly. It records the packet event AND emits the companion gap note for you; do not double-file.

For non-packet substrate gaps, the gap-note path above is the only path.

## Lifecycle

The existing note resolution states are the lifecycle:

- `unresolved` — reported, not triaged
- `acknowledged` — seen, deduped, accepted for later handling
- `addressed` — implemented, rejected, superseded, or intentionally closed

Resolution text should carry the terminal reason:

```text
implemented in commit abc123; added packet predicate WithinWindow
duplicate of note-a1b2c3d4
rejected: application-specific TODO, not blackbox substrate
superseded by roadmap item BB-RX-14
```

When a gap escalates into real implementation work, open or link a `bbox_thread(kind="work_item")` or roadmap item. The note stays as the original field report; the thread becomes the execution object.

## Close-out

The commit that fills a gap should:

- update the supporting code / tests / docs
- resolve the note via `bbox_note_resolve(id="note-…", resolution="addressed")`
- mention the note id in the resolution text
- optionally include a trailer in the commit body:

```text
Addresses-Gap-Note: note-a1b2c3d4
```

Do not delete the note record. Addressed notes are hidden from default views but remain available for provenance and recurrence analysis.
