---
title: "Connectors"
kind: design-hub
corpus: blackbox-design
topic:
  - connectors
brief: "Hub for remote-source connectors: pluggable adapters onto remote file and document stores, mounted as indexable blackbox projects."
---

# Connectors

Connector designs cover the pluggable adapter layer between blackbox's
indexing pipeline and remote file/document stores (Google Drive,
OneDrive/SharePoint, iCloud Drive, WebDAV, S3-compatible stores, and future
sources), including the mountable-project model, sync/freshness semantics,
and the connector catalog.

Credential handling for connectors is owned by the secrets layer:
[Pluggable Secrets Providers](../operations/config-artifacts/secrets-provider.md).

## Docs

- [Remote Source Connectors](remote-source-connectors.md)

## Crosscuts

- [Corpus](../corpus/corpus.md) - the chunker registry, multimodal pipeline,
  and embedding routes that indexed remote content rides.
- [Operations](../operations/operations.md) - config lifecycle and the
  secrets provider layer.
- [Daemon Runtime](../daemon-runtime/daemon-runtime.md) - sync workers live
  under the daemon's actor/concurrency discipline.
