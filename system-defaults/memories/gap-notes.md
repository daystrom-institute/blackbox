+++
title = "Gap notes — report substrate gaps via bbox_gap"
tags = ["gap", "gap-note", "gap-notes", "bbox_gap", "bbox_gaps", "bbox_gap_resolve", "bbox_gap_update", "blackbox.gap_note.v1", "substrate", "substrate-gap", "missing-primitive", "missing-capability", "field-report", "repo-owned", "packet_ast", "tooling", "agent-gap", "workflow-gap", "refactor_primitive", "mcp_surface", "ontology", "eval_coverage", "docs_runbook", "dedupe_key", "impact", "blocking_level", "addresses-gap-note", "close-out", "runbook"]
order = 18
template = false
+++
# Gap notes — report substrate gaps via `bbox_gap`

When the blocker is in the blackbox substrate or shared agent workflow — not in the current product codebase — file a gap note.

Gap notes are first-class: a dedicated, typed, repo-owned store with its own `bbox_gap*` tool family. They are NOT side-channel notes — do not file them through `bbox_note`.

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

The test: would agents in unrelated projects plausibly hit the same missing blackbox capability? If yes, gap note. If no, a normal `bbox_note(kind="followup")`.

## How to file

Call `bbox_gap` with typed parameters (no JSON envelope):

```text
bbox_gap(
  title="Packet AST cannot express rate predicates",
  gap_kind="packet_ast",
  domain="review-policy",
  wanted_capability="Classify entities by count/rate within a time window.",
  dedupe_key="packet_ast/review-policy/rate-window-predicate",
  impact="medium",
  blocking_level="workaround_available",
  missing_primitive="RateCmp / WithinWindow",
  fallback_used="Prose rubric plus manual review.",
  evidence=["packet-event:gap:...", "thread-7f01324e"],
)
```

Required: `title`, `gap_kind`, `domain`, `wanted_capability`, `dedupe_key`.
Optional: `impact` (default `medium`), `blocking_level`, `missing_primitive`, `fallback_used`, `evidence`, `suggested_owner`, `notes`.

Pass `project` (the working dir) and `scope`:

- `scope="project"` (default) → the gap is **repo-owned**: one file per gap under `<project>/.bbox/gaps/gap-<8hex>.json`, committed and travelling with the checkout.

On a transport-governed (locality-cutover) estate the daemon holds no checkout authority, so a project-scoped `bbox_gap` does not write the file directly: it validates, mints the id, dedupes against the served view, and enqueues the exact committed-file bytes for the checkout-owner collector, which applies them within one collector cycle. The tool response says where the file lands; commit it with your change to publish. The same backchannel carries `bbox_gap_update` / `bbox_gap_resolve` and project-scoped knowledge writes (`bbox_learn` / `bbox_remember` / `bbox_decide` / `bbox_forget`).
- `scope="global"` → cross-project substrate gaps that aren't about the current repo land in the central host store.

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

Search open gaps first with `bbox_gaps`, filtering by the typed fields:

```text
bbox_gaps(dedupe_key="packet_ast/review-policy/rate-window-predicate")
bbox_gaps(gap_kind="mcp_surface", domain="transcripts")
```

An open gap with the same `dedupe_key` **dedupes automatically**: `bbox_gap` returns the existing id instead of creating a duplicate. To deliberately tally a recurrence after close-out, pass `allow_recurrence=true`. Addressed gaps are not a dedupe hit — a recurrence after close-out is itself signal. Pass `json=true` to `bbox_gaps` for machine-readable records.

## Editing and supersession

- Amend a gap in place with `bbox_gap_update` (refine title, wanted_capability, impact, evidence, notes, …) — no need to re-file.
- Retire a stale gap in favor of a better-shaped successor with `bbox_gap_resolve(id=…, resolution="addressed", superseded_by="gap-<id>")`. This writes the structured `supersedes` / `superseded_by` link on both records.

## Packet AST gaps

If you are actively authoring a packet and the AST is the missing surface, use `bbox_packet_gap` directly. It records the packet event AND emits the companion gap into the gap store for you; do not double-file.

## Filing without direct MCP access

A caller without `bbox_gap` should hand a `blackbox.gap_note.v1` envelope
(the same typed gap fields) to a caller that has it. Daemon-side file-drop import and Git closeout checks are retired from
`bbox_inbox`, which is a read-only attention preview. Repository checks belong
in the checkout-owning harness. A queued project-gap reply means delivery was
accepted durably; the owner still commits and publishes the delivered changes.

## Lifecycle

The resolution states are the lifecycle:

- `unresolved` — reported, not triaged
- `acknowledged` — seen, deduped, accepted for later handling
- `addressed` — implemented, rejected, superseded, or intentionally closed

Resolve with `bbox_gap_resolve(id="gap-…", resolution="addressed", note="…")`; the resolution text should carry the terminal reason:

```text
implemented in commit abc123; added packet predicate WithinWindow
rejected: application-specific TODO, not blackbox substrate
superseded by roadmap item BB-RX-14
```

When a gap escalates into real implementation work, open or link a `bbox_thread(kind="work_item")` or roadmap item. The gap stays as the original field report; the thread becomes the execution object.

## Close-out

The commit that fills a gap should:

- update the supporting code / tests / docs
- resolve the gap via `bbox_gap_resolve(id="gap-…", resolution="addressed")`
- mention the gap id in the resolution text
- optionally include a trailer in the commit body:

```text
Addresses-Gap-Note: gap-a1b2c3d4
```

Do not delete the gap record. Addressed gaps are hidden from default views but remain available for provenance and recurrence analysis.
