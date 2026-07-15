---
title: "Remote corpus host: dedicated blackbox machine, LAN tunnels, transcript collector"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - corpus
---

# Remote corpus host: dedicated blackbox machine, LAN tunnels, transcript collector

Blackbox and its heaviest consumers currently share one machine with many
concurrently-operating agents. Blackbox holds 5-16GB of RAM depending on
indexing activity, with recurring reindex thrash and peaky embedding-batch
buffering. This design moves the corpus authority to a dedicated always-on
machine (a Mac mini) on the same LAN, keeps execution local to the machines
that own repos and workers, and adds a slim transcript collector so every
source machine's provider transcripts keep reaching the corpus.

All source machines are stationary LAN peers. There is no roaming-laptop
requirement; that assumption simplifies transport and credential handling
throughout.

## Topology

- **Corpus mini (dedicated host):** blackboxd (corpus role) and blackopsd,
  co-located, talking to each other over loopback. blackopsd is the
  operational singleton (agent graph, mailboxes, workflows, schedules); it
  cannot shard per-machine without fracturing that authority, and it belongs
  beside the corpus on the always-on host rather than on a workstation.
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

## Transport and credentials: LAN tunnels, unchanged trust model

Transport is launchd-managed SSH tunnels (autossh-style keepalive) from each
agent machine to the mini; static point-to-point WireGuard is an acceptable
substitute. After blackboxd leaves the workstation, its old loopback port is
free, so the tunnel claims it:

- `127.0.0.1:7264` on each machine forwards to `mini:7264` (corpus).
- `127.0.0.1:7266` forwards to `mini:7266` (blackopsd), where needed.

Consequences, and the reason this beats a mesh VPN or TLS work:

- Every fail-closed loopback guard stays exactly as it is: blackboxd corpus
  bind (`src/server/run.rs`), fleetd bind and URL validation
  (`crates/fleetd/src/config.rs`), blackopsd bind and URL validation
  (`crates/blackopsd/src/config.rs`). Zero relaxation, zero new config
  surface.
- Interactive MCP client configs keep their current `127.0.0.1` URLs.
- fleetd's `blackboxd_url` / `blackopsd_url` stay loopback.
- Worker sandbox port denies and `protected_peer_service_roots` keep working
  unchanged; workers still cannot reach the corpus except through the typed
  worker RPC brokered by their local fleetd.

Bearer auth is unchanged in code. The same `service.token` value is
provisioned out of band to each machine's local owner-only 0600 file. The
file-trust checks in `crates/bro-rpc/src/auth.rs` are local-only and the
comparison is by value, so `auth.rs` needs no modification.

Plain LAN HTTP with a bind allowlist was considered and remains a fallback if
tunnel management proves annoying; it trades guard-relaxation code and
plaintext transcripts on the wire for fewer runtime moving parts. A mesh
overlay (Tailscale) was rejected: it solves a roaming problem this deployment
does not have.

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
that live on agent machines is handled by mounting or mirroring those repos
on the mini through the remote-source connectors design
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
the fat blackboxd on the mini already relieves the agent machines. The
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
| 0. Tunnels + token provisioning | launchd SSH tunnels each machine to mini; shared bearer value in each local 0600 file | - | Low (ops only, no code) |
| 1. Move blackboxd to the mini | The entire RAM relief: HNSW, tantivy, EdgeIndex, reindex/merge/compaction overlap leave the agent machine. Client URLs and guards unchanged | 0 | Low-Med |
| 2. Collector | `bbox-collector` binary; inline-payload ingest kind; per-host producer id; local spool | 0, ingest extension | Med (the new server code) |
| 3. Multi-fleetd identity + blackopsd fan-out | Cross-machine blackopsd-originated dispatch | 0 | High (new protocol surface); defer until needed |
| 4. AR-001 completion | Lean corpus binary on the mini; public corpus MCP served off the peeled state | 1 | Med |
| 5. Repos-on-mini source indexing | Cross-project code search over agent-machine repos via connectors | independent | Low-Med |
