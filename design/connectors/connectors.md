---
title: "Connectors"
kind: design-hub
corpus: blackbox-design
topic:
  - connectors
brief: "Hub for remote-source connectors: file-tree mounts, API-dataset projections, connector actions, custody policy, and graph-native source integration."
---

# Connectors

Connector designs cover the pluggable adapter layer between Blackbox and remote
sources. File-tree connectors mount remote documents as indexable projects.
API-dataset connectors observe typed remote state, project it into source-owned
graphs, expose targeted actions, and place remote bytes according to explicit
custody policy.

Credential handling for connectors is owned by the secrets layer:
[Pluggable Secrets Providers](../operations/config-artifacts/secrets-provider.md).

## Docs

- [Remote Source Connectors](remote-source-connectors.md)
- [Graph-native connector campaign](reflective-graph-connector-program.md)

## Crosscuts

- [Corpus](../corpus/corpus.md) - the chunker registry, multimodal pipeline,
  reflective graph, evidence edges, and embedding routes that remote content
  rides.
- [Operations](../operations/operations.md) - config lifecycle and the
  secrets provider layer.
- [Daemon Runtime](../daemon-runtime/daemon-runtime.md) - sync workers live
  under the daemon's actor/concurrency discipline.
