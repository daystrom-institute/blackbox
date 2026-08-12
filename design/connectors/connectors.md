---
title: "Connectors"
kind: design-hub
corpus: blackbox-design
topic:
  - connectors
brief: "Hub for remote-source connectors: producer-plane observers of remote stores and API datasets, publishing to the corpus host over the collector-style transport; file-tree, API-dataset, and conversation profiles; graph-native source integration."
---

# Connectors

Connector designs cover the pluggable observer layer between remote sources
and the Blackbox corpus. A connector is a producer-plane satellite: it holds
the remote system's credentials, observes the source through change cursors or
checkpoints, applies policy, and publishes content or typed observations to
the corpus host over an authenticated transport. The corpus daemon accepts,
chunks, indexes, projects, and activates; it never fetches remote bytes and
never holds connector credentials. This follows the locality split
(`../daemon-runtime/locality-first-decomposition.md`), which retired the
earlier daemon-side mount/connector substrate; these designs revive the
remote-source capability on the producer axis.

Three profiles share the runtime:

- **File-tree**: remote document stores (Google Drive, OneDrive/SharePoint,
  WebDAV, S3-compatible) published as indexable source content, with
  provider-native documents exported to chunker-claimed formats.
- **API-dataset**: typed business systems (Xero first) observed into
  connector-owned source graphs with targeted evidence actions.
- **Conversation**: message platforms (Slack first) ingested into the
  conversation corpus as searchable transcript-shaped content.

Credential custody is owned by the secrets layer:
[Secret custody across the checkout and corpus planes](../operations/config-artifacts/secrets-provider.md).
Wire authentication rides scope-bound producer tokens, as established by the
code-source collector transport.

## Docs

- [Remote Source Connectors](remote-source-connectors.md) - the file-tree
  profile and the shared transport, identity, policy, and onboarding
  contracts.
- [Slack Ingestion Connector](slack-ingestion-connector.md) - the
  conversation profile: visible Slack messages, corpus-searchable, read-only.
- [Graph-native connector campaign](reflective-graph-connector-program.md) -
  the delivery program tying the reflective graph kernel, source-owned graph
  projections, the Xero profile, evidence bindings, and unified retrieval
  into one arc.

## Crosscuts

- [Daemon Runtime](../daemon-runtime/daemon-runtime.md) - the locality split,
  the collector transport these designs extend, and the onboarding trust
  model (two-sided operator config, no agent self-service).
- [Corpus](../corpus/corpus.md) - the chunker registry, multimodal pipeline,
  reflective graph, evidence edges, and embedding routes that remote content
  rides.
- [Operations](../operations/operations.md) - config lifecycle and the
  secrets provider layer.
- [Integrations](../integrations/integrations.md) - the Slack agent bridge,
  which the Slack ingestion connector complements but does not replace.
