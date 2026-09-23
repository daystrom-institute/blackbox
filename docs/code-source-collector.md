# Code Source Collector

The code source collector publishes current project files and optional complete
typed Git-history snapshots from the machine that owns a checkout to the corpus
daemon. It uploads bounded raw file bytes and canonical commit facts, never Git
packs or an object database. In the reverse direction, optional provenance
export pulls a daemon-authored observed-edge plan and applies it through the
shared checkout-local writer. The daemon remains responsible for chunking,
indexing, embeddings, entity references, graph snapshots, and activation.

In catalog mode, a configured scope that has no project yet remains pending
onboarding. The authenticated collector probes its owning checkout, submits the
recorded identity, and admits the project through the catalog onboarding lane.
Source publication requires the admitted scope's producer grant; enrollment
alone does not authorize reads from arbitrary paths. No matching daemon-local
checkout is required for collected code and transported Git history.

Git-history transport requires every published member of one repo-history
identity to be assigned to the same producer. Verified history sources
materialize and activate through the corpus builder. Provenance export is
project-scoped and does not widen one member's credential to repository siblings.

## Configure the daemon

Create a unique 64-character lowercase hexadecimal token in an owner-only
directory and file. The path must be a real regular file with one hardlink.
For example:

```sh
install -d -m 700 ~/.config/blackbox/code-collectors
openssl rand -hex 32 > ~/.config/blackbox/code-collectors/checkout-host-a.token
chmod 600 ~/.config/blackbox/code-collectors/checkout-host-a.token
```

Add the producer and its exact published scopes to the daemon configuration:

```toml
[code_collection]
enabled = true
git_transport_enabled = true
knowledge_transport_enabled = true
max_manifest_files = 250000
max_manifest_logical_bytes = 5368709120
max_open_uploads_per_producer = 2
retained_generations = 2
unreferenced_blob_grace_hours = 168
stale_warning_hours = 24
max_git_history_commits = 2000000
max_git_history_logical_bytes = 8589934592
max_provenance_documents = 1000000
max_provenance_logical_bytes = 2147483648

[[code_collection.producers]]
producer_id = "checkout-host-a"
token_file = "~/.config/blackbox/code-collectors/checkout-host-a.token"
scopes = [
  { repo_id = "<recorded-repo-id>", bbox_root_relpath = "." },
]
claim_scopes = "none"
```

The daemon fails closed at startup when an enabled token is unsafe, a scope is
assigned twice, or an existing scope is ambiguous. In catalog mode an unregistered
scope can wait for authenticated onboarding; it cannot publish before admission.
Legacy bridge mode still requires scopes to resolve to registered projects.
On SIGHUP, an invalid replacement retains the previous complete assignment and
authentication table.

Producer fields are:

- `producer_id`: the stable id named by authentication and assignment errors.
- `token_file` or `token_files`: the mutually exclusive bearer-token forms.
- `scopes`: operator-pinned published scopes. Pins override durable claims.
- `claim_scopes`: `none` by default, or `unclaimed` to let this producer claim
  an unassigned catalog scope on its first authenticated onboard request.

With `claim_scopes = "unclaimed"`, `scopes` may be empty. A new claim is
accepted only when no other producer owns that scope or any scope with the same
repository id. Claims persist in the daemon's producer claims store and remain
effective if the policy later returns to `none`; the policy gates new claims.
A claimed scope without a catalog project is pending onboarding exactly like a
pinned scope. Bridge mode ignores claims.

Inspect or revoke claims offline with:

```sh
blackbox producer-claims list
blackbox producer-claims revoke \
  --producer checkout-host-a \
  --scope '<recorded-repo-id>/.'
```

Both commands load the daemon configuration to resolve the producer claims
store path. `--config <path>` selects a non-default daemon configuration.

### Rotating a producer's token without a downtime window

`token_file` names exactly one accepted token file. To rotate without a
simultaneous two-sided cutover, replace it with `token_files`, an ordered
list: index 0 is the oldest still-accepted token, and later indices are
staged tokens the daemon will also accept.

```toml
[[code_collection.producers]]
producer_id = "checkout-host-a"
token_files = [
  "~/.config/blackbox/code-collectors/checkout-host-a.token",
  "~/.config/blackbox/code-collectors/checkout-host-a.token.next",
]
scopes = [
  { repo_id = "<recorded-repo-id>", bbox_root_relpath = "." },
]
```

`token_file` and `token_files` are mutually exclusive; configuring both, or
neither, refuses at load. The rotation sequence is add-new, redeploy-producer,
remove-old: create the new token file, add it as a later slot and reload,
point the collector's own `token_file` at the new value, confirm it is
verifying against the new slot, then drop the old file from `token_files` (or
collapse back to a singular `token_file`) and reload again to retire it. Every
successful verification logs which slot matched (by index, never the token
value), so an operator can confirm the fleet has moved off slot 0 before
removing it. `[[source_connectors.producers]]` accepts the identical
`token_file`/`token_files` shape.

## Configure the collector

Copy the same private token file to the checkout host through the operator's
secret-distribution path. Do not place it in the repository. Create a collector
configuration such as:

```toml
server_url = "https://corpus.example.invalid/"
token_file = "/home/operator/.config/blackbox/code-collectors/checkout-host-a.token"
interval_secs = 120
mutation_interval_secs = 10
enroll_roots = ["~/repos"]

[[projects]]
root = "/home/operator/repos/project"
scope = { repo_id = "<recorded-repo-id>", bbox_root_relpath = "." }
git_history = true
provenance = true
published_knowledge = { full_ref = "refs/heads/main" }
```

Operator-authored projects remain in the main configuration. Projects enrolled
by the collector are stored in a sibling sidecar named
`<config-stem>.enrolled.toml` by default. Set `enrolled_projects_file` to use a
different path. The sidecar uses the same `[[projects]]` entries as the main
configuration, and the effective project set is the union of both files. When
the same canonical root or scope appears in both, the main configuration wins
and the collector logs a warning.

`enroll_roots` is empty by default. Each configured path expands `~`, must name
an existing directory, and is canonicalized at load time. These roots bound
daemon-routed enrollment requests. Host-local `add` commands do not require the
target to be under an enroll root.

`interval_secs` controls source collection. Queued gap and knowledge edits
poll independently at `mutation_interval_secs` (default 10 seconds, minimum
1 second), so a long source-scan interval does not delay admitted edits.
Delivery errors back off to at most 60 seconds, or the configured mutation
interval when larger. Delivery changes the checkout; publication still waits
for the configured committed ref and its publication cycle.

The configured root must be the main Git worktree for its clone. The committed
scope at the observed `HEAD` must match the configured scope. Symlinks,
submodules, special files, `.bbox`, build output, and unsupported or oversized
files are not published.

`git_history` defaults to `false`. When enabled, the collector captures every
commit reachable from one exact `HEAD` through the stable no-follow Git
authority, refuses shallow clones, verifies the complete graph locally, and
uses resumable content-addressed upload. Multiple configured projects sharing
one Git common directory publish that repository history only once per cycle.

`provenance` also defaults to `false` and runs on an independent retry lane.
When enabled, the collector first pulls deterministic generation-bound pages
made only from that project's direct observed `EDITED_FILE` and `READ_FILE`
edges, applies them under the repository's shared provenance lock, and posts a
receipt binding the plan inventory to the resulting local notes tip. It then
captures that exact notes-ref generation through the stable no-follow Git
authority and uploads its manifest and content-addressed documents. The corpus
validates V2 target membership or resolves V1 paths against one pinned active
code selector before journaled, idempotent edge publication. Imported explicit
edges and `RAN_BASH` observations are never re-exported. If the observed lane
changes during export paging or before receipt, the collector restarts from
page one; already-written documents are counted as unchanged. Import status is
polled to `active`/`superseded`, while invalid typed targets are quarantined
with a durable diagnostic.

`published_knowledge` is also independently opt-in. It names one full
`refs/heads/*` branch ref. The collector pins that ref, reads both committed
`.bbox/knowledge` and `.bbox/gaps` through the stable no-follow Git authority,
uploads the atomic candidate with resumable content-addressed blobs, and then
re-resolves the ref. Ref movement abandons the capture and retries. Working-tree
files are never used, and a linked worktree remains ineligible.

Enroll a main-worktree repository root or subtree and onboard it immediately:

```sh
bbox-code-collector --config /path/to/code-collector.toml add /path/to/project
```

`add` requires complete, non-shallow history and refuses linked worktrees. It
scaffolds the project-owned `.bbox` files, derives the durable root or subtree
scope, and chooses the published branch ref from `--ref`, `origin/HEAD`, or the
current branch in that order. Git history, provenance, and published knowledge
are enabled for the enrolled entry by default. Disable individual lanes with
`--no-git-history`, `--no-provenance`, or `--no-published-knowledge`.

The sidecar replacement is atomic. Re-adding an enrolled root does not add a
duplicate. The command prints one JSON receipt containing the catalog ids,
scope, published ref, sidecar path, and whether the `.bbox` identity is
committed at that ref. An uncommitted receipt lists the exact repo-relative
scaffolding paths to commit. If immediate onboarding is refused, the sidecar
entry remains enrolled and the receipt includes the daemon HTTP status, error
code, and message before the command exits nonzero. The command does not
commit, push, or write outside `.bbox/` and the sidecar.

Publish once and wait for a terminal generation state:

```sh
bbox-code-collector --config /path/to/code-collector.toml once
```

Run continuously with bounded retry backoff:

```sh
bbox-code-collector --config /path/to/code-collector.toml run
```

`run` checks the main configuration and enrolled-projects sidecar modification
times on the checkout-mutation cadence. A valid replacement atomically becomes
the shared snapshot read by every lane pass. An invalid replacement leaves the
previous snapshot active. Changes to `server_url`, `token_file`, or
`trusted_encrypted_network` are logged but retain their active values until the
collector restarts.

## FreshV2 cutover rehearsal

The GH-F overlap gate has a throwaway, full-path rehearsal that starts from a
fresh catalog, publishes code and complete Git history, restarts the isolated
daemon, transports one nonempty V2 provenance document, and runs the offline
cutover preflight. Build the three debug binaries first, then run:

```sh
BBOX_GIT_CUTOVER_SMOKE_ROOT="$(mktemp -d /tmp/bbox-ghf-smoke.XXXXXX)" \
  scripts/git-transport-cutover-smoke.sh all
```

The script leaves its review artifacts under the throwaway root and always
stops its daemon before preflight and exit.

Remote plain HTTP is rejected and redirects are disabled. Loopback HTTP is
accepted for local smoke tests.

## Ownership transitions

Adding an assignment starts in `warming`. Local source remains active until a
collected generation has been fully staged and atomically selected with its
edge snapshot. The daemon then stops local project-file walking for that
project.

Removing or changing an assignment starts an explicit local cutback. The last
collected generation remains active until a complete local generation stages
successfully. A failed cutback is reported as `cutback_pending`; it never
silently falls back to partial local data.

Staleness also preserves the last good collected generation. Restore the
producer and publish again, or perform a deliberate configuration cutback.

## Health and storage

Run `bbox_doctor` to inspect active generations, staleness, collected versus
local Git `HEAD`, missing or corrupt blobs, failed activation, pending cutback,
and failed retirement. The durable store is under
`<state_dir>/code-sources/`. Upload sessions expire after 24 idle hours, while
active and retained generations remain protected. Blob garbage collection and
retained-generation scrubbing run in the background.

Verified Git-history source records live separately under
`<state_dir>/git-sources/`. Unchanged HEADs are skipped by probe and commit
records are content-addressed, so a new complete snapshot reuses prior commit
records rather than copying an entire history sidecar again. The background
maintenance lane expires uploads after 24 idle hours, keeps the current ready
history plus the configured number of prior generations, honors active
materializer pins, and deletes unreferenced records only after
`unreferenced_blob_grace_hours`. Startup and upload requests never perform the
full record sweep.

The last accepted provenance receipt for each project lives under
`<state_dir>/git-sources/provenance-receipts/`. `bbox_doctor` reports those
receipts plus in-process page, stale-restart, and accepted-receipt counters.
The daemon refuses an observed lane larger than the lower of the configured
`max_provenance_logical_bytes` and the 512 MiB transport scan ceiling before
scanning it. Selected edge inventory is capped at 32 MiB and the cached plan at
64 MiB, with at most four plans resident globally; stale plans retain only a
weak generation check and cannot pin an obsolete full edge index. Collector
page responses are independently capped at 128 KiB before JSON decoding. These
explicit transport limits keep an accidentally huge sidecar from becoming
either a daemon-heap allocation or an unbounded scan; compact or archive the
observed lane before retrying rather than raising daemon memory.

Authenticated provenance imports live under
`<state_dir>/git-sources/provenance-imports/`. Their document bytes are
content-addressed, while a durable per-project acceptance sequence and ready
pointer prevent an older queued note snapshot from replacing a newer one.
Edge preparation is capped at 64 MiB; a larger or semantically invalid import
is quarantined before sidecar publication. The projected active-sidecar size
is checked before the project lane is read, and the lane size is checked again
under its mutation lock, so an oversized estate is refused without scanning or
copying it. Successful publication performs one
atomic streaming merge of the project lane and one bounded edge-index rebuild;
the import becomes Active only after its exact edge-key commitment is visible
in the published read view. If active sidecars exceed the rebuild admission
limit, the daemon refuses before parsing and leaves the import recoverably
`Importing` until the lanes are compacted.
Maintenance retains the current and configured prior import generations plus
unfinished, Active, and quarantined evidence, then reclaims unreferenced
document blobs after the normal grace interval.

When a retained blob is corrupt, the daemon keeps already materialized active
documents readable and requests the missing hash during the next publication.
Do not delete the store to repair one generation. Republish from the owning
checkout or complete a local cutback.
