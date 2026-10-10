# Operating blackbox - day 2 runbook

This is the page for keeping a running daemon healthy. It is deliberately
not the design tour. For graph and retrieval mechanics, see
[Graph And Retrieval Internals](graph-retrieval-internals.md). For index,
embedding, and compaction implementation details, see
[Index And Embedding Internals](index-embedding-internals.md).

## What healthy looks like

Start with the aggregate check:

```text
bbox_doctor(format="summary")
```

`bbox_doctor`, `bbox_stats`, `bbox_embed_status`, `bbox_reindex`,
`bbox_reembed`, `bbox_edge_compact` and `bbox_project_register` are on the
`ops` surface: run them from an `ops` MCP session or with
`bro mcp call <tool> '<json>' --surface ops`.

`bbox_doctor` classifies findings ok/info/warn/action/blocked with suggested
next commands, so a clean run means the drill-down tools below are optional.
When something needs a closer look, or you want to eyeball raw signal
directly, run the individual tools from any MCP client connected to the
daemon:

```text
bbox_stats()
bbox_embed_status()
bbox_project_list()
bbox_doctor(section="snapshots")
bbox_hybrid_search(query="blackbox daemon", limit=5)
```

Healthy output usually means:

| Check | Healthy signal | If not |
|---|---|---|
| `bbox_stats` | Non-zero indexed documents; counts may be cached up to 60 seconds | Check the relevant source publication and search results; totals do not assess source coverage or freshness |
| `bbox_embed_status` | `available: true`, `last_error: null`; queue drains after churn | Fix provider/API key, then `bbox_reembed(route="...")` if needed |
| `bbox_project_list` | Expected repos registered with stable `project_id`s | Register missing repos before blaming search |
| `bbox_doctor(section="snapshots")` | Snapshot manifest present and valid; selected members present; Git overlays pinned in the read view | Check code-source collection and activation |
| `bbox_hybrid_search` | Results include useful refs and sources; project filter works | Check index freshness, embedding status, and project registration |

The daemon's own log is the workload's container log stream.

## After every daemon update

The daemon ships as the runtime image (see `deploy/docker/README.md`): build
and verify the pushed ref, then roll the workload to the verified image
digest. An update covers every component the change touches: once the daemon
is healthy, install the changed checkout-host satellites from the same commit:

```bash
cargo build --release --bin blackbox
cargo build --release -p bro-cli -p bro-harness -p bro-codex -p fleetd \
  -p bbox-code-collector -p bbox-transcript-collector
install -m 755 target/release/blackbox ~/.local/bin/blackbox
install -m 755 target/release/{bro,bro-harness,bro-codex,fleetd} ~/.local/bin/
install -m 755 target/release/{bbox-code-collector,bbox-transcript-collector} ~/.local/bin/
```

Kickstart the collectors after installing them. New sessions pick up a new
`bro-harness` or `bro-codex` without a restart.

Gate the roll with `scripts/converge-gate --drain` and reopen admission with
`scripts/converge-gate --clear` (`docs/converge-gate.md`). The daemon admits
`/admin/*` from loopback or with the admin bearer, so from the operator host
the script sends `Authorization: Bearer` from `--admin-token-file` (default
`~/.local/state/blackbox/admin.token`, the file `converge.sh` reads) and
refuses to run without it.

### Offline project-catalog administration

The `blackbox project-catalog` commands do not contact or restart the daemon.
Use `--config`, `--state-dir`, or `--projects-path` when the bundle does not
come from the normal daemon configuration.

When the daemon is stopped or runs somewhere that cannot read this host's
attachment paths, promote an attached legacy-local project through the offline
surface. The command takes the same exclusive catalog lock and uses the same
committed-`HEAD` attachment proof as the MCP tool:

```bash
blackbox project-catalog promote \
  --projects-path /path/to/state/projects.json \
  --project <project-id> \
  --attachment-id <attachment-id> \
  --expected-catalog-epoch <epoch> \
  --repo-id <recorded-repository-authority> \
  --relpath . \
  --reason "record committed repository authority before remote deployment" \
  --proved-at 2026-01-01T00:00:00Z \
  --config /path/to/config.toml
```

Every active attachment must resolve the proposed scope from its committed
`.bbox/config.toml`; unreadable, unrecorded, or disagreeing attachments make
the command refuse. The resulting migration remains attachment-proved. This is
not the operator-attested unattached migration channel.

`fleetd` is one binary for both instances (see "The fleet supervisor"
below); restart it only when you actually changed it, because restarting it
kills the workers it is supervising.

On macOS, signing and restarting go through `stablesign` and
`launchctl kickstart`. Sign `fleetd` like the other satellites, or the first
daemon dial fails a TCC prompt you never see:

```bash
stablesign ~/.local/bin/fleetd
launchctl kickstart -k gui/$(id -u)/com.daystrom.fleetd
```

Then watch the daemon log. Expected after a normal restart:

- Existing index opens.
- Background reindex starts after its startup delay.
- Embedding queues may receive new/changed docs.
- Before bind, a one-time pass removes the retired edge families
  (`edges/observed/`, `edges/explicit/`, `edges/derived/project/`,
  `edges/migrations/`) and stamps
  `edges/.versions/legacy-edge-lanes-retired-v1`; later starts cost one stat.

Expected after a schema change:

- Log contains `dropping transcript index for schema migration`.
- Reindex takes minutes on a large corpus.

Smoke the daemon after the journal quiets:

```text
bbox_stats()
bbox_embed_status()
bbox_hybrid_search(query="recent changes", project="/abs/path/to/repo", limit=5)
```

## The fleet supervisor (fleetd)

Harness workers are children of `fleetd`, not of `blackboxd`. That is the
whole point: a `blackboxd` restart must not kill live sessions. `fleetd`
changes a few times a year, so its restarts are rare.

**Install.** `fleetd` ships as a workspace binary and installs on the
checkout host (see "After every daemon update"). It runs as its own service:

```bash
# systemd
cp deploy/fleetd.service ~/.config/systemd/user/
systemctl --user enable --now fleetd.service

# launchd (macOS)
cp deploy/fleetd.plist ~/Library/LaunchAgents/com.daystrom.fleetd.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.daystrom.fleetd.plist
```

Running it as a service is recommended, not required. If the socket is absent
when a dispatch needs it, the daemon starts `fleetd` itself, detached so the
supervisor outlives the daemon that started it. It resolves the binary from
`BLACKBOX_FLEETD_BIN`, else a `fleetd` sitting next to the daemon binary, else
`PATH`.

**Local socket and token.** Both derive from the daemon's state dir:

| Path | Contents |
|---|---|
| `<state_dir>/fleetd.sock` | The daemon<->fleetd Unix domain socket |
| `<state_dir>/fleetd.token` | Shared-secret bearer token, owner-only `0600` |

Deriving them from the state dir is what keeps prod and dev on **separate
supervisors**: `~/.local/state/blackbox` and `~/.local/state/blackbox-dev` are
different directories, so the dev daemon cannot adopt prod's sessions or vice
versa. Use `deploy/fleetd-dev.service` / `deploy/fleetd-dev.plist` for the dev
instance; they differ from prod only in `--state-dir`, and that difference is
load-bearing.

Whichever of the two starts first creates the token; the other loads it. If
you ever delete it, stop both, delete it, and start `fleetd` first.

**Re-adoption.** The daemon dials `fleetd` as soon as it starts, without
waiting for a dispatch, and on every connect (that first dial or a reconnect)
asks `fleetd` what it is holding and reattaches each session the task store
knows, replaying from that task's own durable ingest cursor. No dispatch or
resume is served on a new connection until that sweep has finished. An
unreachable `fleetd` at startup is logged and does not stop the daemon; the
next dispatch dials again. So a restart looks like this in the log:

```text
connected to fleetd
re-adopting a fleetd session; replaying from our cursor
fleetd replay complete; session is live
```

Behaviors worth knowing:

- A task the previous daemon marked `Failed` at load with "server restarted
  while task was running" flips **back** to `Running` when its session is
  re-adopted, and that notice is stripped. The child never died; the daemon did.
- `bro_resume` on a session whose task is running, re-adopted or not, does not
  spawn. It returns the running task id with the `bro_wait` and `bro_cancel`
  calls to use instead. It first completes re-adoption, retrying a sweep that
  failed at startup; if `fleetd` still cannot be reached, it refuses without
  starting a worker, because the session may still be live.
- A known task whose live session is **declined** (its workspace binding cannot
  be restored) keeps `Failed`, but the restart notice is replaced by one naming
  the reason and saying the worker is still live under `fleetd`, and the task
  is no longer marked resumable. The worker keeps running; stop it by hand if
  it is no longer wanted.
- A session `fleetd` reports that the task store does **not** know (a TTL reap,
  a wiped store) is logged loudly and **left running**. It is never killed:
  killing work the daemon merely forgot is worse than leaking a process. Kill
  those by hand after checking what they are.

**Executor selection.** `daemon.executor` defaults to `fleetd`. The escape
hatch is explicit and exists for tests and for contributors who have not
installed the supervisor:

```bash
BLACKBOX_EXECUTOR=local   # or daemon.executor = "local" in config.toml
```

There is **no automatic fallback**. If `fleetd` is selected and unreachable,
the dispatch fails loudly rather than quietly spawning a daemon child, because
a silent downgrade would reintroduce the restart-drops-sessions problem
invisibly.

**One remote fleet.** A full daemon running off-host can select one fleetd over
the same bounded, generation-fenced protocol:

```toml
[daemon]
executor = "fleetd"
fleetd_endpoint = "tcp://agent-host.tailnet:7265"
fleetd_token_file = "/run/secrets/fleetd-token"
fleetd_worker_home = "/home/on-agent-host"
fleetd_worker_bro_home = "/state/on-agent-host/bro"
```

The equivalent environment variables are `BLACKBOX_FLEETD_ENDPOINT` and
`BLACKBOX_FLEETD_TOKEN_FILE`, plus `BLACKBOX_FLEETD_WORKER_HOME` and
`BLACKBOX_FLEETD_WORKER_BRO_HOME`. Both worker paths are mandatory absolute
paths for TCP because they name the filesystem where fleetd actually starts
the harness. The token file must already exist, be owned by the daemon uid,
and have no group or other permission bits. A remote client never creates that
file, starts fleetd, or falls back to a local worker.

A `tcp://` endpoint is plaintext: the bearer token is only as private as the
network between the two hosts, so fleetd serves a non-loopback plaintext
listener only behind `--allow-nonloopback-tcp`, on an encrypted,
ACL-restricted transport such as a tailnet. The `tls://` form pins fleetd's
own certificate instead:

```toml
[daemon]
executor = "fleetd"
fleetd_endpoint = "tls://192.168.0.149:7265"
fleetd_tls_fingerprint = "<sha256 printed by `fleetd identity init`>"
fleetd_token_file = "/run/secrets/fleetd-token"
fleetd_worker_home = "/home/on-agent-host"
fleetd_worker_bro_home = "/state/on-agent-host/bro"
```

On the fleetd host, `fleetd identity init --listen-tcp <ip:port>` writes
`fleetd-tls.crt` and `fleetd-tls.key` into the state dir (or
`--tls-identity-dir`) and prints the certificate's SHA-256; fleetd then runs
with `--tls-identity-dir <dir>` (or `BLACKBOX_FLEETD_TLS_IDENTITY_DIR`) and
serves every `--listen-tcp` address over TLS, which grants a non-loopback
address without the plaintext flag. The daemon accepts exactly the
certificate whose digest is `fleetd_tls_fingerprint`
(`BLACKBOX_FLEETD_TLS_FINGERPRINT`); no authority, name or expiry is
consulted, and the token is sent only inside that channel. `fleetd identity
show` prints the digest of an existing identity; `identity init` refuses to
replace one, since the daemon's pin would break. To rotate, remove the two
files, run `identity init` again, update the daemon's fingerprint, and
restart both.

An off-host worker also needs a reachable daemon capability URL. Set
`BLACKBOX_MCP_URL` on blackboxd to the tailnet ingress URL, including the MCP
path, instead of the bind-derived loopback default. Worker event logs remain
authoritative under worker BRO_HOME for replay and resume; blackboxd mirrors
the relayed envelope stream into its own BRO_HOME so corpus indexing remains
daemon-local.

TCP listening is disabled by default. A loopback listener is suitable for a
local encrypted tunnel:

```bash
fleetd --state-dir "$BLACKBOX_STATE_DIR" --listen-tcp 127.0.0.1:7265
```

A direct tailnet bind also requires the second explicit grant:

```bash
fleetd --state-dir "$BLACKBOX_STATE_DIR" \
  --listen-tcp 100.64.0.10:7265 \
  --allow-nonloopback-tcp
```

The TCP bearer protocol does not add TLS. Non-loopback use is valid only on an
encrypted, ACL-restricted transport such as Tailscale, with the listener bound
to that interface address. Do not expose it on a LAN wildcard or public
interface. Unix operation retains the independent peer-uid check. The v1
remote surface selects exactly one fleetd; multi-fleet routing is separate.

**Restart semantics.** Restarting `fleetd` kills its children; that is an
accepted v1 limit, not a bug (process-adoption tricks are not worth it for a
binary this stable). Restarting `blackboxd` does not. Losing the connection in
either direction is not session death: children keep running, the durable event
log keeps accumulating, and the next daemon replays from its cursor.

## Reindexing

The daemon keeps a Tantivy index for transcripts, project files, git
messages, knowledge entries, threads, and tool-call records. The
background reindexer runs periodically, controlled by
`BLACKBOX_REINDEX_INTERVAL_SECS` (default `120`).

Manual reindexing is for operator intervention, not normal file edits.

```text
bbox_reindex(full=false)
```

Use incremental reindex when:

- search looks stale after recent transcript or source changes;
- you restored protected JSON stores and want the index to catch up;
- you registered a project and want to start indexing immediately;
- a background reindex failed after a transient filesystem or lock issue.

```text
bbox_reindex(full=true)
```

Use full reindex when:

- `INDEX_SCHEMA_VERSION` changed;
- chunking/tokenization changed;
- the index was created by an older incompatible binary;
- `bbox_stats` looks impossible, or searches return stale/deleted paths;
- you suspect index corruption.

Watch for:

```text
auto-reindex: indexed N files (M docs)
```

The rebuildable index lives at:

```text
~/.local/share/blackbox/index/
```

Do not back it up as durable state. Rebuild it from transcripts,
registered projects, and protected JSON stores.

## Project registration and code freshness

Project file indexing only covers registered repos. Check before adding:

```text
bbox_project_list()
```

Register with an absolute path on the checkout host (see
[Getting Started](getting-started.md) for collector-backed enrollment):

```bash
bro mcp call bbox_project_register '{"path":"/abs/path/to/repo"}' --surface ops
```

Registration records the root in `~/.local/state/blackbox/projects.json`,
starts an incremental reindex, and nudges the code read view refresher. Large
repos can take 10+ minutes on first index.

If source navigation or refactor tools cannot see a repo, verify in this
order:

1. `bbox_project_list()` includes the repo.
2. `bbox_stats()` shows project-file growth after reindex.
3. `bbox_hybrid_search(query="known symbol", project="/abs/path/to/repo")`
   returns project-file refs.
4. `bbox_embed_status()` shows the `code` route available if you need vector search.

For more code-specific tooling, see
[Projects And Code Indexing](projects-code-indexing.md).

## Embeddings and re-embedding

Embeddings are a second lane beside Tantivy. Reindexing creates source
documents; embedding workers turn those source docs into vector
partitions under:

```text
~/.local/state/blackbox/vectors/
```

Check route health:

```text
bbox_embed_status()
```

Important fields:

| Field | Meaning |
|---|---|
| `available` | Provider/model can currently serve that route |
| `provider`, `model`, `dim` | Active embedding backend and vector shape |
| `indexed_count` | Number of vectors stored for that route |
| `queue_depth` | Pending docs waiting to embed |
| `retried_count` | Retry pressure; should not climb forever |
| `last_error` | First place to look for auth, dimension, or provider failures |

Routes normally include:

| Route | Typical contents |
|---|---|
| `code` | Source code chunks |
| `docs` | Markdown and doc chunks |
| `git_message` | Commit subjects/bodies |
| `knowledge` | Knowledge-store entries |
| `transcripts` | Transcript blocks |

Re-embed a route when:

- provider/model/dimension changed in `~/.config/blackbox/embed.toml`;
- Voyage/Ollama was down and a route accumulated failures;
- vectors were deleted during restore;
- vector search misses content that BM25 finds after reindexing.

```text
bbox_reembed(route="code")
bbox_reembed(route="docs")
bbox_reembed(route="transcripts")
```

Then watch:

```text
bbox_embed_status()
```

`queue_depth` should trend down. A non-zero queue is normal during a
large reindex; a queue that never drains is an operations issue.

Voyage needs `DAYSTROM_VOYAGE_API_KEY` or `VOYAGE_API_KEY` in the daemon's
environment (see [Operations](operations.md#api-keys)). Restart the daemon
after changing it.

## Compaction

There are two different things people mean by compaction. They do not
share the same fix.

| Area | What grows | Normal action |
|---|---|---|
| Vector partitions | WAL records under `~/.local/state/blackbox/vectors/` | Automatic background compactor |
| Edge sidecars | Legacy top-level JSONL sidecars under `~/.local/state/blackbox/edges/` | `bbox_edge_compact` when sidecars grow from repeated full reindex replay |

### Vector compaction

Vector WAL compaction is automatic. You should not normally run a tool
for it. Watch the daemon log for `vector partition compacted` if disk churn
or vector files look suspicious.

If vectors are bad because the provider changed, the operational fix is
not "compact harder"; it is:

```text
bbox_reembed(route="<route>")
```

### Edge sidecar compaction

Legacy top-level edge sidecars can grow when old derived edges are
appended by repeated full refreshes. Compact one project at a time.

First dry-run:

```text
bbox_edge_compact(project_id="d723917f", apply=false)
```

Review removed/retained counts. If the scope is expected, apply:

```text
bbox_edge_compact(project_id="d723917f", apply=true)
```

The tool keeps explicit, provenance and malformed lines and removes legacy
derived edges. It writes a backup before replacing the sidecar.

## Backup and restore boundary

Protect durable JSON stores and installed operator artifacts. Rebuild
indexes, vectors, edge sidecars, and git metadata.

Protect:

- `~/.local/state/blackbox/blackbox-knowledge.json`
- `~/.local/state/blackbox/blackbox-notes.json`
- `~/.local/state/blackbox/blackbox-threads.json`
- `~/.local/state/blackbox/projects.json`
- `~/.local/state/blackbox/project-catalog-migration.json`
- `~/.local/state/blackbox/project-catalog-migration-receipt.json`
- `~/.local/state/blackbox/project-catalog-migration-assets/`
- `~/.local/state/blackbox/packets/`
- `~/.local/state/blackbox/artifacts/`
- `~/.local/state/blackbox/bro/`
- customized `~/.config/blackbox/embed.toml`
- the deployment's secrets, including API keys

Rebuild:

- `~/.local/share/blackbox/index/` with `bbox_reindex(full=true)`
- `~/.local/state/blackbox/vectors/` with `bbox_reembed(route="...")`
- `~/.local/state/blackbox/edges/` via reindex
- `~/.local/state/blackbox/git_meta/` via the next reindex

The longer backup checklist lives in [Operations](operations.md).

### Repairing a blocked repo-file transaction

`error.repo_transaction_recovery_blocked` means an applying transaction lost
or corrupted its manifest. Automatic recovery stops because it cannot prove a
safe roll-forward or rollback.

Before clearing anything, stop writes to that checkout and copy
`<checkout>/.bbox/local/knowledge-transactions/` to operator-controlled backup
storage. Inspect `pending.json`, require `state` to be `blocked`, and record its
exact `transaction_id`. Restore each affected repo-owned corpus file from a
known source, such as the checkout's Git state, the transaction object files,
or an operator backup. Only after the restored files have been reviewed should
the operator remove the exact transaction-id directory and its matching
`pending.json`. Never clear a preparing or applying pointer, and never remove
the transaction root as a whole.

## Troubleshooting quick map

| Symptom | First checks | Likely action |
|---|---|---|
| Search misses recent transcripts | `bbox_stats`, daemon log reindex lines | `bbox_reindex(full=false)` |
| Search returns deleted files | project registration, index age | `bbox_reindex(full=true)` |
| Hybrid search is lexical only | `bbox_embed_status` | Fix route/provider, then `bbox_reembed(route="...")` |
| Code nav cannot see repo | `bbox_project_list` | `bro mcp call bbox_project_register '{"path":"/abs/path"}' --surface ops` |
| Disk grows under `vectors/` | daemon log compaction lines | Usually wait; re-embed only after provider/data issues |
| Disk grows under `edges/` | sidecar size, project id | Dry-run `bbox_edge_compact` |
| Provider markdown stale | rendered files | `bbox_render(scope="global")` on the daemon host; `bro render global` on any other operator host (pulls the plan from a remote daemon) |

## Key paths

These paths belong to the host running the named component. A remote daemon's
stores are not operator-host files; inspect them through its MCP surfaces or
its deployment runbook. Global provider guidance is rendered on the operator
host with `bro render global`.

| Path | Contents |
|---|---|
| `~/.local/bin/blackbox` | Offline administration CLI |
| `~/.local/bin/bro` | Terminal TUI client |
| `~/.local/bin/bro-harness` | Model-turn runtime exec'd by `fleetd` |
| `~/.local/bin/fleetd` | Fleet supervisor binary (shared by prod and dev) |
| `~/.local/bin/bbox-code-collector` | Checkout-source collector |
| `~/.local/bin/bbox-transcript-collector` | Native transcript collector |
| `~/.local/share/blackbox/index/` | Rebuildable Tantivy index |
| `~/.local/share/blackbox/memories/` | Shipped system memories and runbooks |
| `~/.local/state/blackbox/vectors/` | Rebuildable vector partitions |
| `~/.local/state/blackbox/edges/` | Edge sidecars: snapshot manifest, snapshots and overlays |
| `~/.local/state/blackbox/git_meta/` | Rebuildable git fingerprints |
| `~/.local/state/blackbox/fleetd.sock` | Prod daemon<->fleetd socket |
| `~/.local/state/blackbox/fleetd.token` | Prod fleetd shared secret (owner-only) |
| `~/.local/state/blackbox/` | Durable JSON stores plus rebuildable projections |
| `<bro_home>/mcp.json` | Global MCP server config |
| `<project>/.bbox/mcp.json` | Project MCP overlay |
