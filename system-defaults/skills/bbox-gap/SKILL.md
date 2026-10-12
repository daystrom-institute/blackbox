---
name: bbox-gap
description: Dedupe and file a Blackbox gap note, or amend or resolve an existing one. Use when a tool, MCP surface, workflow shape, runbook or instruction was missing, wrong-shaped or misleading and you had to work around it, when the user asks to report or log a gap, limitation or wishlist item, or when deciding whether something is a gap, a thread note or a tracker issue.
---

# Reporting a substrate gap

A gap note is a typed field report that a capability agents needed was missing from Blackbox or the shared agent workflow. Gaps are how the substrate learns what to build next, so one specific, deduplicated gap is worth more than a paragraph of complaint in a final report nobody mines.

## Is it a gap?

Ask: would an agent working in an unrelated project plausibly hit the same missing capability?

| What you have | Where it goes |
|---|---|
| A missing or wrong-shaped Blackbox tool, MCP surface, workflow shape, graph relationship, runbook or rendered instruction that other projects would hit too | A gap note, through `bbox_gap` |
| A follow-up that matters only to this line of work, or feedback about the environment that is not a missing capability | A note on the work `bbox_thread`, or your final report |
| A standing rule or preference the user stated | A proposal to persist it (the `bbox-persist` skill), not a gap |
| A defect, TODO or feature request in the product you are working on | That project's own tracker or your final report |

Agents file gaps, and triage promotes gaps to tracker issues. File a tracker issue directly only when the operator directs it.

## Procedure

1. **Write down the capability, not the symptom**: what you expected to be able to do, what happened, and what you did instead. Keep the receipts and reproducible evidence. A failed lookup is not proof that a service or recipient is dead. Remove private client identifiers before writing into a public repository.

2. **Choose the owner.** `scope="project"` with `project=<working checkout>` for a report that belongs to the repository you are in. `scope="global"` for a cross-project report that no repository owns. That other projects could hit it does not by itself make it global.

3. **Dedupe.** Build the key as `<gap_kind>/<domain>/<capability-slug>`, boring and stable so the next reporter derives the same one, then search:

   ```text
   bbox_gaps(dedupe_key="<key>", include_addressed=true)
   bbox_gaps(gap_kind="<kind>", domain="<domain>")
   bbox_gaps(query="<a word or two from the capability>")
   ```

   An earlier reporter may have chosen a different slug, so broaden by kind, domain and free text. Follow `next_offset` when the list pages. Also check uncommitted `.bbox/gaps/` files in the checkout and gaps you filed earlier this session: a branch-local gap is not in the daemon's view until its branch is published.

4. **Reuse or file.** If an open gap covers it, add your evidence to that gap. `bbox_gap_update` replaces each field you pass, so read the gap with `bbox_gaps(id="gap-...")` and send the existing evidence plus yours. Otherwise file:

   ```text
   bbox_gap(
     title="<missing capability in one line>",
     gap_kind="tooling",
     domain="<subsystem>",
     wanted_capability="<observable behaviour you needed>",
     dedupe_key="tooling/<subsystem>/<slug>",
     scope="project",
     project="<working checkout>",
     impact="medium",
     blocking_level="workaround_available",
     fallback_used="<what you did instead>",
     evidence=["<file:line, thread id or other ref you read>"],
   )
   ```

   Required: `title`, `gap_kind`, `domain`, `wanted_capability`, `dedupe_key`. Always fill `fallback_used`: the workaround tells triage what the gap costs.

5. **Follow the report to where it lands.** Keep the returned gap id and receipt. A project gap is a file under `.bbox/gaps/` in the owning checkout. On a project whose checkout another host owns, the write is queued and a collector applies it there, so a queued reply proves admission, not delivery or publication. Inspect the returned target, commit only this task's gap file by explicit path when you are authorized to commit, and say what is still pending when you are not. A gap filed from a linked worktree stays branch-local until that branch reaches the published ref; until then `bbox_gaps`, dedupe, update and resolve do not see it, so do not conclude it was lost.

6. **Report** the gap id, title and scope, and whether it is queued, delivered or published. If `bbox_gap` is unavailable in your session, return the proposed fields and name the missing tool. Do not claim a saved report.

## Field values

**`gap_kind`** (new filings accept only these; `packet_ast` appears on old records and is refused for new ones)

- `tooling`: missing CLI, shell or refactor helper
- `agent`: a dispatchable agent that should exist and does not
- `workflow`: missing wait, fork, cancel or sequencing shape
- `refactor_primitive`: language-specific refactor operation
- `mcp_surface`: a tool surface too narrow, too wide or wrongly shaped for a recurring role
- `ontology`: missing entity type or relationship
- `eval_coverage`: a test or eval harness cannot reach a class of cases
- `docs_runbook`: missing or wrong guidance, runbook or rendered instruction

**`impact`**: `low` (nuisance), `medium` (repeated friction, the default), `high` (blocks useful automation or causes recurring bad agent behaviour), `critical` (unsafe edits, data-loss risk or an unusable workflow).

**`blocking_level`**: `none`, `workaround_available`, `blocks_task`, `blocks_class_of_work`.

## Duplicates and recurrence

An open gap with the same key and owner dedupes automatically: `bbox_gap` returns the existing id. `allow_recurrence=true` bypasses that and creates a second open gap, so do not use it to get around deduplication. An addressed gap is not a dedupe hit, and a recurrence after closure needs no flag.

## Amending and resolving

- Amend in place with `bbox_gap_update(id=..., <fields>)` instead of re-filing, minding that each field is replaced.
- Resolve only a gap your own verified change fills, or one the operator tells you to resolve: `bbox_gap_resolve(id="gap-...", resolution="addressed", note="<verified outcome, with the commit or the rejection reason>")`. The other states are `unresolved` and `acknowledged`.
- Close in favour of a better-shaped successor with `superseded_by="gap-<new>"`, which links both records.
- Never delete a gap record. Addressed gaps leave default listings and stay available for provenance.
- For a project gap, an update or resolve changes the same repo-owned file and follows the same delivery and commit path as a new filing.
