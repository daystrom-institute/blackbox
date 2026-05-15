---
title: "Evented Coordination"
kind: design-hub
corpus: blackbox-design
topic:
  - operations
  - events
brief: "Hub for system events, event journal, reaction registry, outbox, and event-driven coordination."
---

# Evented Coordination

System events are the daemon-wide eventing substrate for inbound signals,
outbound reactions, retry, dead-letter handling, and integrations such as Slack
or Forgejo-backed workflows.

## Docs

- [System Events](system-events.md)
- [System Events - Implementation Plan](system-events-impl.md)

## Crosscuts

- [Orchestration](../../orchestration/orchestration.md)
- [Slack](../../integrations/slack/slack.md)
- [Surfaces](../../surfaces/surfaces.md)
