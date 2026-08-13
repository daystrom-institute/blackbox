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
brief: "An opt-in, default-off, per-project grant that lets a Ready publication candidate from a project's already-bound producer be accepted on the linear fast path, through the exact acceptance path the operator tool uses, with the grant read from the currently accepted pointer so a candidate can never authorize itself."
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

- acceptance authority moves to the OPERATOR, ahead of time, per project,
  through an audited act;
- the producer gains no new capability whatsoever. It uploads and
  finalizes exactly as before, and cannot enable, widen, read, or infer
  the grant;
- no model is anywhere on this path. The trigger is the daemon's own
  finalize handler;
- the grant is scoped to a lane the operator already reviewed once: the
  same bound producer, the same catalog scope, the same published ref.
  Everything else still requires an operator.

The honest way to state the change: the operator's approval moves from
per-generation to per-lane. That is a real reduction in review
granularity and the operator should choose it explicitly, which is why the
feature is opt-in, default off, and revocable.

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

### 2.2 Chosen: operator-set metadata on the accepted-publication pointer

The grant is a field on `AcceptedPublicationPointerV1`:

```text
auto_advance: Option<{ enabled: bool, granted_reason: String }>
```

It is set only by an explicit `auto_advance` parameter on
`bbox_project_publisher_advance`, and `granted_reason` is that call's own
bounded `audit_reason`.

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

## 3. The activation rule

> A policy attempt reads the grant from the pointer that is CURRENTLY
> accepted. Only an operator advance writes that pointer's grant.
> Therefore the candidate being accepted can never be what authorizes its
> acceptance.

Consequences that fall out of the rule rather than being enforced
separately:

- **Enabling takes one operator advance.** The operator passes
  `auto_advance=true` on an advance (or an establish). That call is
  ordinary operator authority with full CAS tokens and an audit reason.
  The FIRST candidate the policy may accept is the next one.
- **Establish is never automatic.** With no installed pointer there is no
  grant to read, and the attempt reports `no_accepted_publication`.
- **A policy acceptance cannot widen itself.** The policy path always
  passes `AutoAdvanceGrantUpdate::Inherit`, which carries the operator's
  grant forward unchanged. `Set` exists only on the operator parameter.
- **Revocation is symmetric.** `auto_advance=false` on any later operator
  advance clears the grant.

## 4. Scope: the linear fast path only

An attempt proceeds only when all of these hold, checked against the
accepted pointer:

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
any other non-linear move stay manual by construction: none of them is
reachable from `PublisherPublishMode::Advance` with the current pointer's
own tokens.

## 5. Reuse, not a parallel path

`publish_from_ready_candidate` is the single candidate-acceptance path.
`bbox_project_publisher_advance` and the policy trigger both call it, so
"the policy validates identically" is structural rather than a claim about
two similar functions. It returns `PublishError` rather than `anyhow` so
the operator tool keeps `may_have_swapped()` and its post-failure
reconvergence.

The policy caller differs from the operator caller in exactly three ways:
its mode is always `Advance` with tokens read from the pointer it is
replacing, its grant update is always `Inherit`, and its audit reason is
generated rather than supplied.

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
- **The prior accepted generation keeps serving.** The policy only ever
  calls the ordinary acceptance path, which swaps a pointer or refuses;
  there is no partial state between them.
- **A refusal never fails the upload.** The producer's finalize succeeded
  regardless.
- **Every exit records a reason.** A candidate sitting unserved with
  nothing said anywhere is the failure this design exists to end;
  replacing it with an unexplained skip would reproduce it.

On success the policy performs the same post-swap convergence the operator
tool does: invalidate the projected caches, converge the published
knowledge index, refresh published graph views, and record the accepted
publication mutation observation.

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

- No producer-supplied policy, in any encoding, over any route.
- No model on the acceptance path.
- No establish, rollback, scope change, or producer rebind by policy.
- No retry, backoff, or queue. One attempt, then the operator.
- No durable per-attempt history. The ledger is bounded and in-process.
- No change to accepted content, its normalization, or its hashes.
