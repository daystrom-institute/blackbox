---
title: "Daemon Runtime"
kind: design-hub
corpus: blackbox-design
topic:
  - daemon-runtime
brief: "Topic home for Blackbox runtime architecture: process topology across blackboxd, blackopsd, fleetd, and bro-harness workers, plus each service's Tokio topology, execution planes, lock discipline, persistence actors, and restart behavior."
---

# Daemon Runtime

Designs for the runtime architecture itself: process authority across blackboxd,
blackopsd, fleetd, and bro-harness workers; the Tokio topology inside each process; the
division of work into isolated planes; lock and persistence discipline; and the
actor patterns that keep blocking work off async workers.

Distinct from [Orchestration](../orchestration/orchestration.md), which owns what
is dispatched, and [Bro-Harness](../bro-harness/bro-harness.md), which owns the
agent loop itself. This topic owns process placement, scheduling, isolation,
restart, and migration between runtime authorities.

## Documents

- [Process topology: corpus, operations, fleet, and session workers](process-topology.md)
- [Blackops service boundary: operational intent outside the flight recorder](blackops-service-boundary.md)
- [Fleet extraction: strangling live execution out of blackboxd](fleet-extraction.md)
- [Agent runtime program: from Codex findings to independent services](agent-runtime-program.md)
- [Concurrency model: planes, invariants, and the path off the bolt-on era](concurrency-model.md)
- [Bro-harness worker protocol](../bro-harness/worker-protocol.md)
