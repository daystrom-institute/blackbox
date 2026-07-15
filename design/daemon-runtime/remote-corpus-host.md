---
title: "Remote corpus host: corpus on the cage, LAN transport, transcript collector"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - corpus
---

# Remote corpus host: corpus on the cage, LAN transport, transcript collector

Blackbox and its heaviest consumers currently share one machine with many
concurrently-operating agents. Blackbox holds 5-16GB of RAM depending on
indexing activity, with recurring reindex thrash and peaky embedding-batch
buffering. This design moves the corpus authority off the agent machines,
keeps execution local to the machines that own repos and workers, and adds
a slim transcript collector so every source machine's provider transcripts
keep reaching the corpus.

The corpus host is the operator's k3s homelab cluster ("the cage": one
controller plus four 96GB workers on 10GbE, Longhorn replicated block
storage across per-worker NVMe, Flux/Pulumi converge control, and an
observability stack; CAGE-V2 phases 0-5 complete 2026-07-12). A dedicated
always-on Mac was evaluated as the host and remains a workable stopgap
(section: Alternative host), but the cluster wins on RAM headroom, storage,
reconciliation, and monitoring, and the operator already offloads work to
it.

All source machines are stationary LAN peers. There is no roaming-laptop
requirement; that assumption simplifies transport and credential handling
throughout.

## Topology

- **Corpus on the cage:** blackboxd (corpus role) and blackopsd run as
  cluster workloads, co-located in one pod or adjacent pods so their
  mutual traffic stays cluster-internal. blackopsd is the operational
  singleton (agent graph, mailboxes, workflows, schedules); it cannot shard
  per-machine without fracturing that authority, and it belongs beside the
  corpus on the always-on host rather than on a workstation. State lives on
  Longhorn-replicated volumes; the tantivy index, HNSW vector partitions,
  and EdgeIndex sidecars are rebuildable but expensive, so replicated
  volumes are the right default.
- **Each agent machine:** its own fleetd plus the bro-harness workers fleetd
  launches. Fleet travels with the harness and the repos. Workers keep
  connecting to their local fleetd over the private per-host Unix socket;
  nothing about the worker protocol changes.
- **Each source machine:** the transcript collector (below), which requires
  no fleetd and no bro plane. A machine running only interactive claude or
  codex CLIs ships transcripts with just the collector installed.

This differs deliberately from `design/bro-harness/remote-worker-boundary.md`,
which places a worker remote from its fleetd (private-FS worker over an
off-host socket). Here every fleetd stays co-located with its workers and
repos; only fleetd-to-corpus and blackopsd-to-fleetd links cross hosts. The
private-clone and workspace-identity problems of the remote-worker design do
not arise.

## Transport and credentials

The corpus services bind non-loopback inside the cluster and are exposed on
the LAN through the cluster's existing ingress/LoadBalancer surface. That
requires a deliberate, opt-in relaxation of the loopback fail-closed guards,
which stay fail-closed by default:

- blackboxd corpus-role bind (`src/server/run.rs`) and blackopsd bind
  (`crates/blackopsd/src/config.rs`) gain an explicit non-loopback opt-in
  (config field plus env override) intended for containerized deployment.
- fleetd's `blackboxd_url` / `blackopsd_url` validators
  (`crates/fleetd/src/config.rs`) and blackopsd's `blackboxd_url` validator
  gain the same opt-in so agent machines can point at the cluster service
  directly. SSH tunnels from each agent machine to a node (claiming the
  freed local 7264/7266 ports) remain a supported alternative that keeps
  every client-side URL loopback; use them if the wire should be encrypted.
- Worker sandbox rules are unchanged in intent: workers still cannot reach
  the corpus except through the typed worker RPC brokered by their local
  fleetd. The corpus endpoint (host:port or tunnel port) joins the denied
  set the same way the local ports do today.

Bearer auth is unchanged in code. The same `service.token` value is
provisioned to each machine's local owner-only 0600 file and to the cluster
as a secret through the homelab's established secret-sourcing path (never
committed). The file-trust checks in `crates/bro-rpc/src/auth.rs` are
local-only and the comparison is by value, so `auth.rs` needs no
modification. Plaintext HTTP on the switched LAN is an accepted risk under
the single-trusted-operator threat model; tunnels are the documented
upgrade if that changes. A mesh overlay (Tailscale) was rejected: it solves
a roaming problem this deployment does not have.

## Deployment and estate placement

Corpus deployment follows the homelab's converge discipline: Linux container
images for blackboxd (corpus role) and blackopsd, manifests reconciled by
Flux, state on Longhorn volumes, health probes on `/healthz` / `/readyz`,
and the existing observability stack scraping the services so reindex and
embedding behavior is finally graphed. The infra repo's self-description
currently scopes it to the PlanGlobal estate; landing blackbox manifests
there requires the operator's one-line amendment to that scope note so the
repo's docs stay truthful, or a small separate overlay repo targeting the
same cluster. Either is fine; the choice is the operator's.

## Alternative host: a dedicated Mac

A dedicated always-on Mac on the LAN behind launchd-managed SSH tunnels was
the original shape of this design and remains viable, notably as a stopgap
that needs zero code changes (tunnels claim the freed loopback ports, so no
guard relaxation at all) and runs the same darwin binaries built daily. It
gives up the cluster's RAM headroom, replicated storage, reconciliation,
and monitoring. Cutover between hosts either direction is a state copy plus
retargeting the client transport; the collector and ingest work below is
identical for both.

## Credential posture (operator decision, 2026-07-15)

The worker sandbox denies each worker its own provider lane's credential
source and the service token; it does not attempt cross-lane or
whole-home credential isolation. That is intentional. The threat model is
accident containment for a single trusted operator: the credentials present
on these machines are read-only and/or exactly the ones meant to be available
for automation. Broader isolation mechanics (denying `~/.ssh`, `~/.aws`,
sibling provider lanes) are out of scope; they add failure modes where agents
mis-handle or destroy tokens while delegating, without protecting against a
threat the deployment actually has. Do not reintroduce cross-lane credential
denial without a new operator decision.

## The transcript collector

A new slim standalone binary (working name `bbox-collector`), shaped like a
log shipper (Filebeat/Alloy): tail transcript roots, keep a durable local
registry/cursor, push increments to the corpus host with at-least-once
delivery, spool locally while the corpus is unreachable, catch up on
reconnect. It links the already-peeled transcript adapters and cursor store
from `crates/bbox-corpus-index/src/transcripts/` as a library; it must not
link tantivy, vectors, EdgeIndex, or V8. A blackboxd "shipper role" was
rejected because the `blackbox` crate drags the whole corpus stack onto
machines that only need tailing.

Adapter-specific cursor semantics carry over from the existing code rather
than a generic byte-offset registry:

- Append-only JSONL sources (claude, harness session logs) use byte offsets,
  shipping only through the last complete newline. The strict fleet reader
  (`read_fleet_event_log_prefix`) is the model; the lenient interactive
  adapters' skip-torn-tail-and-advance behavior is a known gap the collector
  must not copy.
- Gemini-style whole-JSON snapshot sources keep the seen-message-id-set
  cursor.
- No logrotate/inode machinery: provider session files do not rotate.

### Wire contract

The server side largely exists: `blackbox-corpus-service` record ingestion
(`crates/blackbox-corpus-service/src/records.rs`) is durable, idempotent
(`record_id` dedupe, conflict on same-id-different-content), and
producer-cursored (strictly contiguous per producer, compaction-tolerant
first attach). Two extensions are required:

1. An inline-payload transcript-increment record kind. The existing
   transcript specialization reads `transcript_path` from the corpus host's
   local disk under `allowed_roots`; a remote producer's file is not on that
   disk, so the committed bytes travel in the record payload instead.
2. Per-machine producer identity: a stable host id stamped into the producer
   field (for example `collector:<host-uuid>`). The record path namespaces
   per producer, so two machines with identical project paths cannot collide
   on the wire. The local cursor-store fingerprint stays host-local and
   unchanged.

## What does not move

Working-set truth stays on the machine that owns the repo, per
`design/bro-harness/remote-worker-boundary.md`: per-worktree LSP sessions,
code navigation, refactor runners, validation, file tools, and the V8
isolates inside bro-harness workers. Cross-project source indexing of repos
that live on agent machines is handled by mirroring those repos to the
corpus host through the remote-source connectors design
(`design/connectors/remote-source-connectors.md`), not by shipping live
source deltas.

## Why the RAM math works

Embedding compute is already remote (Voyage HTTP API); the local embedding
cost is batch buffering only. The 5-16GB is blackboxd itself: in-RAM HNSW
vector partitions (multi-GB, warmed at boot, transiently doubled during
compaction), tantivy writer heap plus live segments plus merge overlap
(recurring on the reindex interval), and the fully-in-memory EdgeIndex.
Moving the blackboxd process moves all of it. The offload unit is the whole
process; there is no useful partial "embedding offload" because there is no
local embedding compute to offload.

AR-001 (`ARCH_RELAYER_LOG.md`) gates the lean corpus binary, not the move:
the fat blackboxd on the corpus host already relieves the agent machines. The
`/internal/records` and capability surfaces the collector and fleetd use are
already dependency-clean in `blackbox-corpus-service`; only the public
agent-facing corpus MCP remains coupled to legacy `SharedState`.

## Deferred: multi-fleetd identity and routing

Nothing in `bro-core`, `bro-protocol`, or `fleet-core` carries a machine or
fleet-instance identity today; blackopsd holds a single `fleetd_url`; the
cockpit targets one fleetd. Worker-originated calls fan in to the central
blackopsd fine (each fleetd holds a blackopsd bearer and forwards), so
multi-machine execution works without new protocol as long as
blackopsd-originated dispatch (schedules, crons, workflow steps choosing a
machine) is not required across machines. When it is, that needs: a fleet
instance identity minted in the contract bottom, a fleetd registry in
blackopsd replacing the single URL, and a machine-routing key on execution
requests. That is genuinely new design surface, unaddressed by any current
doc, and is deliberately the last slice.

## Slices

| Slice | Delivers | Depends on | Risk |
|---|---|---|---|
| 0. Linux images + bind/URL opt-in + secret provisioning | Container builds of blackboxd (corpus role) and blackopsd; explicit non-loopback opt-in on the bind and URL guards (fail-closed default); bearer value provisioned to machines and cluster | - | Low-Med (small code, mostly build/ops) |
| 1. Corpus on the cage | Flux-reconciled deployment on Longhorn volumes; the entire RAM relief: HNSW, tantivy, EdgeIndex, reindex/merge/compaction overlap leave the agent machines; observability scrapes the corpus | 0 | Low-Med |
| 2. Collector | `bbox-collector` binary; inline-payload ingest kind; per-host producer id; local spool. Day-one for the cage: no transcripts originate on the cluster, and collector catch-up from cursor zero doubles as the transcript migration path | 0, ingest extension | Med (the new server code) |
| 3. Multi-fleetd identity + blackopsd fan-out | Cross-machine blackopsd-originated dispatch | 0 | High (new protocol surface); defer until needed |
| 4. AR-001 completion | Lean corpus binary; public corpus MCP served off the peeled state | 1 | Med |
| 5. Repo mirroring for source indexing | Cross-project code search over agent-machine repos via connectors | independent | Low-Med |
