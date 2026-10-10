---
title: "Harness and daemon boundary"
kind: design
corpus: blackbox-design
topic:
  - bro-harness
brief: "Process ownership and dependency boundaries for provider execution."
---

# Harness and daemon boundary

The daemon owns dispatch admission, task persistence, corpus access and worker
supervision. Provider execution runs in a child process owned by the selected
executor. Checkout-host workers run under fleetd; a local executor owns its own
children.

The daemon does not link `bro-harness`, `bro-code-mode`, V8 or
`bro-capabilities`. `bro-core` owns shared identifiers and provider/origin types;
`bro-protocol` owns worker, control and observation contracts. Provider-native
adapters normalize events into the shared worker protocol.

The headless `bro` CLI reaches the daemon over HTTP. Shared connection and
dispatch configuration lives in `bro-config`, without task mirrors, transcript
rendering or an execution runtime. Task ownership stays with the daemon.

The detailed contracts are:

- [Harness process boundary](harness-process-boundary.md): child process control,
  capability access, credentials and event persistence.
- [Provider lanes](../orchestration/provider-lanes.md): provider-specific launch
  and control adapters.
- [Task control plane](../daemon-runtime/control-plane.md): HTTP task operations,
  origins, roster observation and managed-worktree closeout.
