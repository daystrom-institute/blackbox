---
title: "Corpus Retention and Resident-State Bounds"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - storage
tags:
  - retention
  - edge-sidecar
  - vectors
brief: "Define one bounded lifecycle for primary corpus inputs, indexes, vectors, and edge sidecar state."
---

# Corpus Retention and Resident-State Bounds

## Problem

Blackbox bounds several secondary stores, but its primary corpus lifecycle is
not governed by one retention contract. Transcript documents, vector records,
and Git-history projections can continue accumulating as their source histories
grow.

Allocator tuning and storage garbage collection can reduce operational pressure,
but they do not define which primary facts remain hot, which move to cold
storage, or which are deliberately forgotten. Snapshot count and byte budgets
solve a separate derived-snapshot problem and do not close this question.

The missing capability is tracked by `gap-0d3219e8`. Process and vector-memory
observability remains a separate concern in `gap-bcced6fb`.

## Required Invariant

One policy must govern every representation of retained corpus history. A fact
outside the retained set must not survive accidentally in Tantivy, a vector
partition, or an edge sidecar merely because that store has a different cleanup
path.

The policy must be applied consistently at:

- ingestion, so newly discovered old material follows the declared contract;
- reindex, so rebuilds do not resurrect evicted material;
- background maintenance, so a long-running daemon converges without restart;
- diagnostics, so operators can distinguish retained, cold, pending-eviction,
  and inconsistent state.

## Design Questions

### Retention unit

Choose whether policy is expressed primarily as age, bytes, entity count, or a
combination. A byte ceiling gives a predictable resource bound, while an age
window gives a predictable recall contract. If both exist, the ordering and
floor rules must be explicit.

### Recall contract

Choose one of three user-visible outcomes for material outside the hot set:

1. permanent removal;
2. a cold searchable tier with slower retrieval;
3. a cold archive that must be explicitly rehydrated.

Silent partial recall is not acceptable. Search and inspection tools must disclose
when a query can only see the hot set.

### Cross-store transaction

Eviction spans independently durable stores. The design needs an idempotent
journal or equivalent transaction record so crashes cannot leave a document
searchable after its source is evicted or a vector active after its source
document is gone.

### Git-history policy

Git ancestry and changed-file edges have different semantics from transcript
history. A shallow hot projection may preserve recent provenance while a cold
commit source remains reconstructible from authenticated repository history.
The policy should exploit that reconstructibility rather than treating all
modalities identically.

## Non-Goals

- Reopening the already-addressed workspace-snapshot budget work.
- Treating allocator selection as the retention policy.
- Conflating missing memory diagnostics with unbounded retention.
- Encoding temporary production measurements or host topology in the design.

## Acceptance Shape

A completed design should specify:

- configuration fields and precedence;
- the retained-set calculation and its floors;
- the user-visible recall contract;
- a crash-safe cross-store eviction protocol;
- reindex and boot behavior;
- diagnostics for policy drift;
- migration behavior for existing unbounded state;
- tests proving that repeated ingest, restart, and reindex stay within the
  declared bound without resurrecting evicted facts.
