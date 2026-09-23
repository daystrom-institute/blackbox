---
title: "Publisher auto-advance: an operator-granted acceptance policy"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - knowledge
  - corpus
tags: [publisher, accepted-publication, producer, policy, gap-a6911d0e]
brief: "Default-off operator grants for establishing and advancing accepted publication from a project's owning producer through the exact operator acceptance path: producer-level auto_publish establishes the first pointer on the first candidate's full branch ref, then the pointer's auto-advance grant governs the linear fast path."
---

# Publisher auto-advance

> **Status: partial.** The policy, the shared acceptance path, the trigger,
> the ledger, and the status surface are implemented. Not yet done: a
> durable audit record of the triggering reason (section 7), and a live
> exercise against a real collector.

## 0. The problem

The collector uploads a Ready knowledge-source publication candidate and
then nothing serves it. Acceptance requires an operator to call
`bbox_project_publisher_advance` with three compare-and-swap tokens.
Observed live: a committed graph generation reached durable terminal
success collector-side and sat unserved indefinitely, with nothing in any
status surface saying why.

The operator cost per routine commit is a status read, three tokens, and a
tool call. On a continuously running collector that cost is paid on every
knowledge commit, which is how "the pointer is stale" becomes the normal
state of a project rather than an incident.

## 1. What this narrows, and why that is not a contradiction

`knowledge-source-transport-impl.md` section 10 lists as a non-goal:

> No automatic knowledge acceptance by a producer or model.

That non-goal is deliberate and this design does not reopen it. It
narrows it, and the narrowing turns on WHO decides.

The non-goal forbids the PRODUCER (or a model) from gaining acceptance
discretion. What it protects is the property that a checkout owner cannot
make blackboxd serve content nobody approved. This feature leaves that
property intact:

- acceptance authority moves to the OPERATOR ahead of time, either per
  project through an audited publisher act or per producer through daemon
  config;
- the producer gains no new capability whatsoever. It uploads and
  finalizes exactly as before, and cannot enable, widen, read, or infer
  the grant;
- no model is anywhere on this path. The trigger is the daemon's own
  finalize handler;
- the grant is scoped to the owning producer and the catalog scope. The first
  candidate must name a full branch ref, which becomes the pointer's ref.
  Later advances remain bound to that exact ref;

The operator's approval moves from per-generation to per-lane. The
producer-level `auto_publish` grant covers only the first pointer for
projects that producer currently owns. The pointer installed by that
establish carries the per-project auto-advance grant for later generations.
Both policies are opt-in and default off.

Rejected framing: "the producer is trusted now". It is not. A granted
project still refuses a candidate from a different producer, a different
scope, or a different ref, and still runs the full acceptance validation
on the bytes.

## 2. Where the policy lives, and the two candidates considered

The binding constraint is that **the grant must not be self-activating**:
whatever authorizes an acceptance must not be something the candidate
being accepted supplies.

### 2.1 Rejected: the accepted generation's committed project config

The natural-sounding home is a `[publisher] auto_advance = true` table in
the project's committed `.bbox/config.toml`, read from the CURRENTLY
ACCEPTED generation. The self-activation problem is solved by the
sequencing: a config change only takes effect after one manual advance has
accepted a generation carrying it, and that manual advance is the audited
operator grant.

It was rejected for two reasons.

**Cost.** The accepted generation carries three source lanes (knowledge,
gaps, graphs) and no project config. Reading a committed config from it
means a fourth lane through `bbox-knowledge-source`'s descriptor and
limits, `bbox-knowledge-source-store`'s manifest/blob/finalize path, the
collector's capture, `AcceptedPublicationBuildInputV1`, the immutable
generation shape, its hashes and counts, and every golden id that binds
them. That is a transport-contract change to carry one boolean.

**Authority shape.** The grant would then be producer-attested bytes: the
daemon would read a blob the producer uploaded to decide whether to trust
blobs the producer uploads. The one-manual-advance rule does rescue the
safety argument, but it makes the operator's grant an implicit
consequence of accepting a commit rather than an explicit act. An operator
reading an audit trail should see the moment they granted acceptance, not
have to infer it from a config diff inside an accepted generation.

### 2.2 Accepted-pointer metadata for continuing auto-advance

The grant is a field on `AcceptedPublicationPointerV1`:

```text
auto_advance: Option<{ enabled: bool, granted_reason: String }>
```

It is set by an explicit `auto_advance` parameter on
`bbox_project_publisher_advance`, or by a successful producer-level
`auto_publish` establish. An operator call uses its bounded
`audit_reason`; auto-publish uses
`policy:auto_publish producer=<id>`.

Why the pointer and not the catalog record (`CorpusProject`): the catalog
was the other operator-owned candidate, and it would work. It was rejected
on blast radius and on fit. `CorpusProject` is a `deny_unknown_fields`
struct with 66 construction sites and its own validation, migration,
genesis, and rebuild paths; adding a field there to express a fact about
accepted publication puts publication state in the catalog. The pointer,
by contrast, is the object the feature already has to respect: it is the
compare-and-swap anchor, it is written by exactly one code path, and it
already holds the source binding the policy has to match against.

Properties this buys:

- **Additive and inert.** The field is `Option` with `serde(default,
  skip_serializing_if)`. Every pointer written before this feature encodes
  byte-identically and keeps its `pointer_sha256`, which is a live
  compare-and-swap token.
- **One locked read.** `auto_advance_grant()` returns the grant, the CAS
  tokens, the accepted scope, the published ref, and the source binding
  from one read of one pointer under the publication lock. Reading the
  grant and the tokens separately would let an advance land between them,
  and a policy attempt would present tokens for a pointer whose grant it
  never checked.
- **Unreachable by the producer.** No transport route writes a pointer.

### 2.3 Producer config for the first pointer

`auto_publish = true` on one `[[code_collection.producers]]` entry is an
operator-authored pre-grant. It applies only while that producer is the
project's effective owner through a config pin or durable claim. A Ready
candidate qualifies only when its producer is that owner, its scope is the
project's catalog scope, its full ref is a non-empty `refs/heads/...` branch,
and the project has an attached, repo-knowledge capable attachment with the
same validated scope. The attachment's checked-out `branch_ref` does not
constrain publication. The first candidate's branch ref becomes the pointer's
ref.

The daemon establishes through `publish_from_ready_candidate` with
`PublisherPublishMode::Establish` and
`AutoAdvanceGrantUpdate::Set { enabled: true, ... }`. The ordinary
acceptance path performs candidate validation, catalog epoch checks,
source revalidation, and the pointer swap. The installed pointer therefore
contains both the accepted producer binding and the standing auto-advance
grant.

## 3. The activation rule

> Continuing auto-advance reads the grant from the pointer that is CURRENTLY
> accepted. That pointer grant comes from an operator publisher act or from
> operator daemon config authorizing the first auto-publish establish.
> Candidate bytes can never authorize their own acceptance.

Consequences for continuing auto-advance:

- **Enabling takes one operator advance.** The operator passes
  `auto_advance=true` on an advance (or an establish). That call is
  ordinary operator authority with full CAS tokens and an audit reason.
  The FIRST candidate the policy may accept is the next one.
- **Establish requires the separate producer pre-grant.** With no installed
  pointer and no `auto_publish` grant on the effective owner, the attempt
  reports `no_accepted_publication`.
- **Continuing auto-advance cannot widen itself.** It passes
  `AutoAdvanceGrantUpdate::Inherit`, which carries the operator's grant
  forward unchanged. The auto-publish establish passes `Set` only because
  operator config already authorized that producer.
- **Revocation is symmetric.** `auto_advance=false` on any later operator
  advance clears the grant.

## 4. Scope: first publication and the linear fast path

With no pointer, auto-publish proceeds only when all of these hold:

| Condition | Otherwise |
|---|---|
| The effective owner has `auto_publish = true` | `no_accepted_publication` |
| The candidate's producer is the effective owner | `producer_mismatch` |
| The candidate's scope is the catalog scope | `scope_changed` |
| The candidate's ref is a non-empty full branch ref | `ref_changed` |
| An attached repo-knowledge capable attachment has the catalog scope | `ref_changed` |
| This candidate has not been attempted | `already_attempted` |

With a pointer, continuing auto-advance proceeds only when all of these hold,
checked against the accepted pointer:

| Condition | Otherwise |
|---|---|
| A pointer is installed | `no_accepted_publication` |
| Its grant is `enabled` | `policy_disabled` |
| Its source binding is `Producer` | `binding_not_producer` |
| It does not already name this candidate | `already_accepted` |
| The candidate's producer matches the bound producer | `producer_mismatch` |
| The candidate's scope equals the accepted scope | `scope_changed` |
| The candidate's `full_ref` equals the accepted ref | `ref_changed` |
| This candidate has not been attempted | `already_attempted` |

Establish, rollback to a prior arm, scope migration, producer rebind, and
any other non-linear move stay manual by construction. The only automatic
establish is the no-pointer `auto_publish` case. Once any pointer exists,
including a disabled or rolled-back pointer, that establish path is closed.
Continuing policy moves remain reachable only through
`PublisherPublishMode::Advance` with the current pointer's own tokens.

## 5. Reuse, not a parallel path

`publish_from_ready_candidate` is the single candidate-acceptance path.
`bbox_project_publisher_advance` and the policy trigger both call it, so
"the policy validates identically" is structural rather than a claim about
two similar functions. It returns `PublishError` rather than `anyhow` so
the operator tool keeps `may_have_swapped()` and its post-failure
reconvergence.

The continuing auto-advance caller uses `Advance` with tokens read from the
pointer it is replacing, passes `Inherit`, and generates
`policy:auto_advance producer=<id> source=<generation>`.

The first-publication caller uses `Establish`, passes `Set` with
`enabled=true`, and generates `policy:auto_publish producer=<id>`. It runs
only after proving that no pointer exists, the candidate matches the effective
owner and catalog scope, its ref is a full branch ref, and an eligible
attachment exists. The accepted pointer records that first candidate's ref;
normal auto-advance binds every later candidate to it.

## 6. Trigger, failure, and the no-storm rule

The trigger is the daemon's publication finalize handler, immediately
after the store makes the generation Ready. It runs before the finalize
response so a producer that polls status right away cannot observe an
unserved candidate the daemon was already accepting.

- **At most one attempt per uploaded candidate**, claimed in a bounded
  in-process ledger BEFORE the attempt, so a failure consumes the claim
  too. A repeated finalize of the same upload reports
  `already_attempted`.
- **No retry, ever.** A refusal logs once at warn and stops. The operator
  advances manually after a refusal.
- **The prior accepted generation keeps serving, unless the refusal
  reached the swap.** The policy only ever calls the ordinary acceptance
  path, which swaps a pointer or refuses. The one case with no clean
  either/or is a failure raised at or after the atomic pointer
  replacement: the new pointer is durably installed and the attempt still
  reports an error, which is what `PublishError::may_have_swapped()`
  names. The policy carries that flag through its refusal outcome and
  converges on it, exactly as the operator tool does.
- **A refusal never fails the upload.** The producer's finalize succeeded
  regardless.
- **Every exit records a reason.** A candidate sitting unserved with
  nothing said anywhere is the failure this design exists to end;
  replacing it with an unexplained skip would reproduce it.

Whenever the pointer moved (an acceptance, or a refusal that reached the
swap) the policy performs the same post-swap convergence the operator tool
does: invalidate the projected caches, converge the published knowledge
index, and refresh published graph views. An acceptance additionally
records the accepted publication mutation observation. Converging is not a
retry: it touches projections only and never re-enters the acceptance
path, so the no-storm rule above is unaffected.

Graph views are the projection with no second chance. Knowledge and gaps
rebuild on read, so a missed convergence heals itself on the next request;
`project_graph_views` serves whatever was last installed, so a missed (or
degraded) refresh keeps serving the previous generation until the next
accept or a daemon restart. That is also why the refresh refuses to
install a prior-arm read over an installed view: a verified read falls
back to the pointer's prior arm whenever the current generation does not
verify, and latching that fallback into the graph read surface is
indistinguishable, to a reader, from an accept that never happened.

Converging on this path is necessary and not sufficient, because the
accept is not the only writer of that view. Overlay recomputation, a
provisional capture, and the boot pass all install published views too,
each from accepted content IT resolved, and each spends real time between
resolving and installing. With collectors cycling every couple of minutes
across a dozen projects, an acceptance lands inside one of those windows
routinely, and the slower caller then reinstalls the superseded view on
top of the fresh one. So the ordering rule lives at the install site
rather than on this path: a view whose accepted generation is not the one
the pointer currently names may not replace a view that is already
serving, whichever caller built it. The pointer is the authority because
generation ids are content digests with no order, and keying the gate on
the pointer is also what keeps it from latching a stale view forever: the
next install for the pointer's own generation is admitted.

## 7. Observability, and one honest gap

`bbox_project_publisher_status` reports an `auto_advance` object:

```text
grant:        { enabled, granted_reason, eligible_binding }
last_attempt: { source_generation_id, producer_id, outcome, ... }
```

`grant` is durable (it is a pointer fact). `last_attempt` is
in-process and bounded: it answers "what did the policy just do", not
"what has it ever done". The durable answer to the latter is the accepted
pointer's own producer binding, which names the exact source generation.
For an auto-published first pointer, `grant.granted_reason` durably exposes
`policy:auto_publish producer=<id>`.

**Gap.** `bbox_project_publisher_advance` does not thread `audit_reason`
into any durable record today. It is a structured log field and a response
field only; scope migration records an operator reason durably, publisher
advance does not. The policy therefore stamps
`policy:auto_advance producer=<id> source=<generation>` into the same log
line the operator advance uses (`tool = "publisher_auto_advance"`,
`"catalog administration mutation"`) and into the ledger, but not into a
durable audit store, because no such store exists for this operation. If a
durable publisher audit trail is wanted, it should be added for BOTH
callers at once rather than only for the policy lane.

## 8. Non-goals

- No producer-supplied policy, in any encoding, over any route. The
  producer-level grant is operator daemon config.
- No model on the acceptance path.
- No establish except the operator-configured `auto_publish` first pointer.
  No rollback, scope change, or producer rebind by policy.
- No retry, backoff, or queue. One attempt, then the operator.
- No durable per-attempt history. The ledger is bounded and in-process.
- No change to accepted content, its normalization, or its hashes.
