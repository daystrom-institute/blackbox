---
title: "Daemon Runtime"
kind: design-hub
corpus: blackbox-design
topic:
  - daemon-runtime
brief: "Topic home for blackboxd's runtime architecture: the tokio topology, execution planes, lock discipline, and persistence actors."
---

# Daemon Runtime

Designs for the daemon's execution architecture itself — the tokio runtime
topology, the division of work into isolated planes (dispatch/harness,
indexing/embedding, coordination stores, control), lock and persistence
discipline, and the actor patterns that keep blocking work off async workers.

Distinct from [Orchestration](../orchestration/orchestration.md) (what the
daemon dispatches) and [Bro-Harness](../bro-harness/bro-harness.md) (the agent
loop itself): this topic is about how the daemon process schedules and isolates
all of that work.

## Documents

- [Concurrency model: planes, invariants, and the path off the bolt-on era](concurrency-model.md)
- [Health-probe starvation during code-source ingest](healthz-ingest-starvation.md)
- [Locality-first decomposition: the checkout plane and the corpus plane](locality-first-decomposition.md)
- [Checkout-plane provenance export implementation plan](checkout-provenance-export-impl.md)
- [Distributed code-source collector implementation plan](distributed-code-source-collector-impl.md)
- [Code-source locality cutover: collected authority, no checkout fallback](code-source-locality-cutover-impl.md)
- [Durable corpus project catalog implementation plan](durable-project-catalog-impl.md)
- [Durable project catalog Phase 1 implementation plan](durable-project-catalog-phase1-impl.md)
- [Durable project catalog Phase 2 implementation plan](durable-project-catalog-phase2-impl.md)
- [Durable project catalog Phase 3 implementation plan](durable-project-catalog-phase3-impl.md)
- [Durable project catalog Phase 4 implementation plan](durable-project-catalog-phase4-impl.md)
- [Durable project catalog Phase 5 implementation plan](durable-project-catalog-phase5-impl.md)
- [Durable project catalog Phase 6 implementation plan](durable-project-catalog-phase6-impl.md)
- [Typed Git-history and provenance transport implementation plan](git-history-provenance-transport-impl.md)
- [Publisher auto-advance: an operator-granted acceptance policy](publisher-auto-advance.md)
