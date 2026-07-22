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
- [Locality-first decomposition: the checkout plane and the corpus plane](locality-first-decomposition.md)
- [Checkout-plane provenance export implementation plan](checkout-provenance-export-impl.md)
