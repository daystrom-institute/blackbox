---
title: "Note-backed gap log implementation plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
  - knowledge
superseded_by: "first-class repo-owned gap store (src/gaps.rs, bbox_gap* tools, committed .bbox/gaps/); see sm-gap-notes"
---

# Note-backed gap log implementation plan

Status: implemented and archived after `32d15e7`
Date: 2026-05-12
Related: `design/corpus/knowledge/note-backed-gap-log.md`

## Scope

Implement the note-backed gap log without adding `bbox_gap` tools or a new gap
store. The implementation is a thin layer over:

- `bbox_note`
- `bbox_notes`
- `bbox_note_resolve`
- `bbox_inbox`
- optional `.bbox/gaps` file-spool ingestion

The first useful milestone is not a perfect lifecycle system. It is a reliable
path for agents in other projects to report substrate gaps in one shape, and
for the operator to see those reports in the normal attention surface.

## Phase 0 - Freeze The Envelope

Goal: make the report format explicit enough that agents can emit it before
any code changes land.

Work:

- Keep `design/corpus/knowledge/note-backed-gap-log.md` as the canonical design.
- Add a short system-memory runbook, or extend the side-channel notes runbook,
  with the `blackbox.gap_note.v1` JSON body and routing rule.
- Keep the runbook imperative:
  - use `bbox_note(kind="followup")`
  - put the JSON envelope in `body`
  - include `project`, `task_id`, `session_id`, `provider`, `bro`, and
    `thread_id` when available
  - do not invent `bbox_gap`
- Do not add `metadata` to `NoteParams` in this phase.

Acceptance:

- An agent that only reads rendered/system memory can file a valid gap note.
- `bbox_notes(kind="followup", query="blackbox.gap_note.v1")` finds it.
- No MCP tool catalog changes.

## Phase 1 - Parse Gap Notes Internally

Goal: centralize gap-note parsing so inbox, future importers, and tests share
one interpretation.

Add a small internal parser module for gap-note bodies. Prefer a dedicated
`gap` submodule if `notes.rs` is split into a module during implementation
(`src/notes/gap.rs`); otherwise keep the parser in a clearly marked gap-note
section of `src/notes.rs` and move it when the file is next split. Do not leave
the parser duplicated in inbox, packet, and importer code.

```rust
pub struct GapNoteView<'a> {
    pub note: &'a Note,
    pub title: String,
    pub gap_kind: Option<String>,
    pub domain: Option<String>,
    pub impact: GapImpact,
    pub blocking_level: Option<String>,
    pub dedupe_key: Option<String>,
}
```

Implementation rules:

- Try `serde_json::from_str::<serde_json::Value>(&note.body)` first.
- Accept only object bodies with `type == "blackbox.gap_note.v1"`.
- Treat malformed JSON as "not a gap note", not an error.
- For hand-authored bodies, optionally fall back to substring detection only for
  discovery/debug views, not for fields used in ordering.
- Keep `gap_kind`, `domain`, `blocking_level`, and `dedupe_key` as advisory
  strings.
- Parse `impact` into an internal rank:
  - `critical`
  - `high`
  - `medium`
  - `low`
  - unknown/missing -> `medium`

Tests:

- compact JSON body parses.
- pretty JSON body parses.
- missing `type` is ignored.
- malformed JSON is ignored.
- unknown impact ranks as `medium`.

Acceptance:

- One helper answers "is this note a gap note?"
- No existing `bbox_notes` output changes.
- No note-store schema migration.

## Phase 2 - Inbox Gap Section

Goal: make reported gaps visible without polluting the ordinary followup
section.

Update `src/inbox.rs`:

- Add a `## Gap notes` section before ordinary `## Followups`.
- Include unresolved and acknowledged gap notes.
- Exclude gap notes from the ordinary followup list so each note appears once.
  This exclusion must use the same Phase 1 gap-note parser used to build the
  gap section. Either add a predicate parameter to `unresolved_notes_of` or
  build a followup-specific helper that filters out `GapNoteView::parse(note)`
  matches after collecting notes.
- Preserve project filtering.
- Sort by:
  1. unresolved before acknowledged
  2. impact rank descending
  3. created_at descending

Suggested row format:

```text
  note-a1b2c3d4 [high packet_ast] transcript-search — Packet AST cannot express rate predicates (packet_ast/review-policy/rate-window-predicate)
```

If fields are missing:

- title fallback: truncate body
- gap_kind fallback: `gap`
- impact fallback: `medium`
- project fallback: `-`

Tests:

- gap notes appear in `Gap notes`.
- gap notes do not also appear in `Followups`.
- acknowledged gap notes remain visible.
- addressed gap notes are hidden.
- project filter works.
- ordering honors resolution, impact, then recency.

Acceptance:

- `bbox_inbox(project=...)` shows gap notes as their own section.
- Ordinary followup view is not noisier after phase 2.

## Phase 3 - Packet Gap Bridge

Goal: keep `bbox_packet_gap` useful while preventing packet AST gaps from
living only in the packet event log.

Update the packet gap path so `bbox_packet_gap` still records the packet event,
then optionally emits a companion gap note.

Companion note body:

```json
{
  "type": "blackbox.gap_note.v1",
  "title": "Packet AST gap: <domain or ast_feature_requested>",
  "gap_kind": "packet_ast",
  "domain": "<domain>",
  "wanted_capability": "<description>",
  "missing_primitive": "<ast_feature_requested>",
  "fallback_used": "<fallback_used>",
  "impact": "medium",
  "blocking_level": "workaround_available",
  "evidence": ["packet-event:gap:<timestamp>"],
  "dedupe_key": "packet_ast/<domain>/<ast_feature_requested-or-description-slug>",
  "notes": "<attempted_sketch>"
}
```

Guardrails:

- Do not require callers to double-file.
- If note creation fails, packet gap logging should still succeed and include a
  warning in the response.
- Avoid duplicate companion notes by checking for an unresolved gap note with
  the same `dedupe_key`. No wall-clock window is needed: addressed notes mean
  the prior occurrence was closed, while unresolved or acknowledged notes mean
  the existing field report is still live.
- Avoid holding the packet store lock while creating the companion note. The
  packet event should be appended first, then the packet lock dropped, then the
  notes lock acquired. This keeps the bridge from introducing a packets/notes
  lock-order dependency on `SharedState`.

Tests:

- `bbox_packet_gap` still returns the existing success shape.
- A valid companion followup note is created.
- Note creation failure does not lose the packet event.
- Existing packet event query behavior remains unchanged.

Acceptance:

- Packet-specific AST gaps are visible in `bbox_packet_events(op="gap")`.
- The same gap class is visible in `bbox_inbox`.

## Phase 4 - File Spool Importer

Goal: let agents without MCP report gaps through files.

Add an importer that scans:

```text
<registered-project>/.bbox/gaps/inbox/*.json
~/.local/share/blackbox/gaps/inbox/*.json
```

Importer shape:

- Prefer a manual/admin command or explicit daemon maintenance hook before
  adding automatic startup mutation.
- Validate that each file is a JSON object with
  `type == "blackbox.gap_note.v1"`.
- Infer `project` from the registered project root for project-local files by
  asking `ProjectRegistry` for the canonical registered root that contains the
  spool path. Do not infer by string slicing alone; symlink aliases and project
  renames must follow the registry's canonical path rules.
- Create `bbox_note(kind="followup")` with the raw JSON as `body`.
- Move successfully imported files to `imported/`.
- Move invalid files to `rejected/` with a sidecar `.error.txt`, or leave them
  in place and report diagnostics without looping noisily.

Idempotency:

- Compute an import fingerprint from path + file hash.
- Store imported fingerprints in a small state file under blackbox state, or
  detect duplicates by `dedupe_key` plus identical `wanted_capability`.
- Never create infinite duplicate notes from the same file.

Tests:

- valid project-local file imports and moves to `imported/`.
- valid host-wide file imports.
- invalid JSON does not create a note.
- repeated importer run is idempotent.
- project inference works for registered project roots.

Acceptance:

- Non-MCP agents have a documented filesystem reporting path.
- Importer failures are visible but do not corrupt notes.

## Phase 5 - Close-out Hygiene

Goal: make gap resolution cheap enough that implemented gaps do not stay open.

Add helper behavior without changing the canonical note lifecycle:

- A lint/check command can report unresolved high-impact gap notes older than N
  days.
- A commit-message scanner can look for:

```text
Addresses-Gap-Note: note-a1b2c3d4
```

- The scanner should verify the note exists and is addressed, but it should not
  auto-resolve without explicit operator action.
- A future helper may offer a dry-run list of candidate close-outs.

Tests:

- scanner recognizes canonical trailer.
- missing note id is reported.
- addressed note passes.
- unresolved note is reported as still open.

Acceptance:

- Implementers get a cheap check before pushing substrate changes.
- The source of truth remains `bbox_note_resolve`.

## Phase 6 - Reporting And Aggregation

Goal: turn accumulated field reports into prioritization signal.

Add read-only summaries over existing notes:

- group by `gap_kind`
- group by `domain`
- group by `dedupe_key`
- count unresolved vs acknowledged vs addressed
- show oldest open and newest open occurrence

This can start as an inbox subsection or CLI/admin command. It does not need a
new MCP write surface.

Acceptance:

- The operator can answer "which missing primitives keep recurring?"
- The answer is derived from notes, not a parallel gap database.

## Implementation Order

Recommended first PR:

1. Phase 1 parser helper.
2. Phase 2 inbox section.
3. Tests for parser and inbox grouping.
4. System-memory/runbook update from Phase 0.

Recommended second PR:

1. Phase 3 `bbox_packet_gap` companion note bridge.
2. Tests for packet event preservation and companion-note creation.

Recommended third PR:

1. Phase 4 file spool importer.
2. Tests for idempotency and project inference.

Recommended later work:

- Phase 5 close-out checks.
- Phase 6 aggregation.
- Revisit `NoteParams.metadata` only if JSON-in-body becomes painful in real
  traffic.

## Risks

- Followup overload: phase 2 should land before broad rollout.
- Duplicate reports: dedupe keys help, but triage discipline is still required.
- JSON-in-body friction: acceptable for v1; revisit after real reports.
- Packet-gap dual path: phase 3 should bridge `bbox_packet_gap` before relying
  on inbox as the only gap dashboard.
- Spool importer mutation: keep importer explicit until the failure modes are
  boring.

## Non-Regression Rules

- Do not add `bbox_gap`.
- Do not add a new note kind unless there is a separate note-taxonomy design.
- Do not make gap notes rendered policy memory.
- Do not make file-spool records canonical after import.
- Do not require agents to double-file packet gaps.
- Do not hide addressed notes from explicit `bbox_notes(include_addressed=true)`
  queries.
