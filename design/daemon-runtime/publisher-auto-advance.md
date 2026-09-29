---
title: "Candidate acceptance: the configured ref is the only gate"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic:
  - daemon-runtime
  - knowledge
  - corpus
tags: [publisher, accepted-publication, producer, retention, gap-a6911d0e]
brief: "Every Ready publication candidate from a project's bound producer, on its accepted scope and configured ref, is accepted as it finalizes; the first valid candidate from the owning producer establishes the pointer. The operator tool only rebinds the producer or ref, moves the scope, or rolls back. Accepted generations beyond the current one, the prior arm, and the two most recent previous ones are collected by maintenance."
---

# Candidate acceptance

## 0. The rule

Merging to a project's configured ref is the only gate on what the daemon
serves. The checkout owner's collector captures that ref into an immutable
Ready candidate; the daemon accepts it as the candidate finalizes. No grant,
policy flag, or per-generation operator act stands between a valid candidate
and the accepted pointer.

"Valid" is the validation every publish runs, unchanged:

- producer authentication and the producer's transport grant for the scope;
- the catalog's current published scope;
- candidate byte integrity (content-addressed manifests and blobs, pinned
  for the whole acceptance);
- the configuration lane parsing in the daemon's configuration domain;
- knowledge, gap, graph, evidence, and configuration normalization into one
  immutable generation.

A candidate that fails any of these is refused whole and the prior accepted
generation keeps serving.

## 1. What is accepted

With an installed pointer, a candidate advances it when all of these hold:

| Condition | Otherwise |
|---|---|
| The pointer is bound to a producer | `binding_not_producer` |
| The pointer does not already name this candidate | `already_accepted` |
| The candidate's producer is the bound producer | `producer_mismatch` |
| The candidate's scope is the accepted scope | `scope_changed` |
| The candidate's full ref is the pointer's ref (the configured ref) | `ref_changed` |
| This candidate has not been attempted in this daemon lifetime | `already_attempted` |

With no pointer, the first valid candidate establishes one when all of these
hold:

| Condition | Otherwise |
|---|---|
| A producer owns the project's catalog scope (config pin or durable claim) | `no_owning_producer` |
| The candidate's producer is that owner | `producer_mismatch` |
| The candidate's scope is the catalog scope | `scope_changed` |
| The candidate's ref is a non-empty `refs/heads/...` branch ref | `ref_not_branch` |
| An attached, repo-knowledge capable attachment carries the catalog scope | `no_attached_checkout` |

The attachment proves the project was admitted for publication (remote
onboarding registers one). Its checked-out branch does not constrain
publication: the establishing candidate's branch ref becomes the configured
ref, and every later candidate is bound to it.

## 2. What waits for the operator

The refusals in the first table other than `already_*` are valid content that
would change the producer, the scope, or the configured ref. The daemon does
not make those moves on its own; the candidate waits, its refusal is recorded,
and `bbox_project_publisher_advance` makes the move:

| Operation | Moves the pointer to | Refuses |
|---|---|---|
| `rebind` | a candidate from a different producer or ref, whose ref becomes the configured ref; a producer candidate for an attachment-bound pointer; or the first pointer for a project whose first candidate was refused | a candidate on the bound producer and ref (acceptance serves it), and any candidate while the catalog scope differs from the accepted scope |
| `scope_move` | a candidate at the catalog's current scope after a scope migration, which clears the bridge | a project with no pointer, and a project whose scopes already agree |
| `rollback` | a specific earlier Ready candidate from the bound producer, scope, and ref | any other producer, scope, or ref, the candidate already served, and a project with no pointer |

A rollback holds until the next candidate finalizes, which is accepted as
usual. The remedy for bad content on the ref is a revert merged to the ref;
rollback serves known-good content while that lands.

For an uncovered project (no knowledge transport row), `rebind` and
`scope_move` also accept `attachment_id` with `full_ref`, publishing that
attached checkout at that ref. Covered projects refuse the attachment arm.

The tool reads the installed pointer, checks the operation against it, and
advances with that pointer's compare-and-swap tokens. A candidate accepted
between the read and the swap turns the move into a pointer-conflict refusal
instead of a silent overwrite; the operator reads status and repeats the move
if it still applies. `expected_catalog_epoch` guards catalog authority the
same way.

## 3. One acceptance path

`publish_from_ready_candidate` is the single candidate-acceptance path. The
finalize trigger and the operator tool both call it, so a candidate validates
identically whichever caller accepts it. It returns `PublishError` rather than
`anyhow` so both callers keep `may_have_swapped()` and its post-failure
reconvergence.

Automatic acceptance advances with the tokens of the pointer its checks read,
so two finalizes racing for one project cannot both swap: the loser refuses as
a pointer conflict and the winner's generation serves.

## 4. Trigger, failure, and the no-storm rule

The trigger is the daemon's publication finalize handler, immediately after
the store makes the candidate Ready. It runs before the finalize response so a
producer that polls status right away cannot observe an unserved candidate the
daemon was already accepting.

- **At most one attempt per uploaded candidate**, claimed in a bounded
  in-process ledger BEFORE the attempt, so a failure consumes the claim too.
- **No retry, ever.** A refusal logs once at warn and stops. The next
  candidate from the collector is the next attempt.
- **The prior accepted generation keeps serving, unless the refusal reached
  the swap.** A failure raised at or after the atomic pointer replacement
  leaves the new pointer installed while reporting an error, which is what
  `PublishError::may_have_swapped()` names. Acceptance carries that flag and
  converges on it, exactly as the operator tool does.
- **A refusal never fails the upload.** The producer's finalize succeeded
  regardless.
- **Every exit records a reason.** A candidate sitting unserved with nothing
  said anywhere is the failure the ledger exists to prevent.

Whenever the pointer moved (an acceptance, or a refusal that reached the swap)
acceptance performs the post-swap convergence: invalidate the projected
caches, converge the published knowledge index, and refresh published graph
views. Converging touches projections only and never re-enters the acceptance
path.

Graph views are the projection with no second chance. Knowledge and gaps
rebuild on read, so a missed convergence heals itself on the next request;
`project_graph_views` serves whatever was last installed. That is also why
the refresh refuses to install a prior-arm read over an installed view: a
verified read falls back to the pointer's prior arm whenever the current
generation does not verify, and latching that fallback into the graph read
surface is indistinguishable, to a reader, from an acceptance that never
happened.

The acceptance is not the only writer of that view. Overlay recomputation,
provisional capture, and the boot pass install published views too, each
from accepted content it resolved, and each spends real time between
resolving and installing. So the ordering rule lives at the install site: a
view whose accepted generation is not the one the pointer currently names may
not replace a view that is already serving, whichever caller built it. The
pointer is the authority because generation ids are content digests with no
order, and keying the gate on the pointer keeps it from latching a stale view
forever: the next install for the pointer's own generation is admitted.

## 5. Index convergence reads the published view

Accepted-publication index convergence (after a swap and in the boot pass)
rebuilds the project's knowledge scope from the `published` view: accepted
content only. It never reads peer provisional snapshots, whose leases expire
on their own schedule; a convergence that read them would record a degraded
peer read for every expired lease on every swap and would index transient
peer state. Provisional content stays visible through the `own` and `all`
views that request it.

## 6. Accepted generation retention

Every acceptance installs a new immutable generation. The store maintenance
pass (hourly, first at startup) collects the ones nothing needs. Per project,
it keeps:

- the pointer's current generation;
- the pointer's prior arm, always, even when it is older than the rest;
- the two most recently written other generations (generation ids are
  content digests with no order, so recency is the file's modification time);
- every generation an in-flight preparation registered, and the generation a
  cached read pins.

Everything else is removed. A project with no pointer is never collected, and
a pointer that does not decode fails that project's step without touching its
files: absent or unreadable authority is not proof that nothing references a
generation. Only regular files named `<generation id>.json` are candidates,
so interrupted atomic-replace temporaries and anything unexpected are left
alone.

Each step removes at most 64 files under one publication-lock hold, and a
pass removes at most 512, so a large backlog drains over a few passes without
holding the lock for long. The in-flight registry is held across a step: a
preparation registers its generation before checking for an existing
content-addressed file, so it either registered first (and its file is
protected) or runs after the step and writes the file afresh. Removing an
already-removed file is a no-op, so the pass is idempotent and a second run
over a drained store does nothing.

## 7. Legacy state

- **Pointer grant field.** A pointer may carry an `auto_advance` object. It
  decodes and is ignored, whatever its shape, and a new pointer never writes
  it. A legacy pointer keeps its installed bytes, and therefore its
  `pointer_sha256`, until the next swap replaces it.
- **Producer config.** `auto_publish` under `[[code_collection.producers]]`
  parses and has no effect.

## 8. Observability

`bbox_project_publisher_status` reports `acceptance.last_attempt`
(`source_generation_id`, `producer_id`, `outcome`, refusal `code` and
`detail`, `may_have_swapped`, `at_unix_secs`); `detail=acceptance` returns it
as exact bounded pages. The ledger is in-process and bounded: it answers "what
did acceptance just do". The durable answer is the pointer itself, whose
producer binding names the exact source generation it serves.

Each acceptance logs one `catalog administration mutation` line with
`tool = "candidate_acceptance"` and an audit reason of the form
`acceptance:<establish|advance> producer=<id> source=<generation>`. Operator
moves log the same line with `tool = "bbox_project_publisher_advance"`, the
operation, and the operator's `audit_reason`.

## 9. Non-goals

- No acceptance authority for the producer or a model: the producer uploads
  what the configured ref holds and cannot choose the ref, the scope, or the
  pointer.
- No automatic producer, scope, or ref change; those are operator moves.
- No retry, backoff, or queue for refused candidates.
- No durable per-attempt history; the ledger is bounded and in-process.
- No change to accepted content, its normalization, or its hashes.
