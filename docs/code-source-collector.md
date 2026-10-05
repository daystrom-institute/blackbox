# Code Source Collector

The code source collector publishes current project files and optional complete
typed Git-history snapshots from the machine that owns a checkout to the corpus
daemon. It uploads bounded raw file bytes and canonical commit facts, never Git
packs or an object database. The daemon remains responsible for chunking,
indexing, embeddings, entity references, graph snapshots, and activation.

In catalog mode, a configured scope that has no project yet remains pending
onboarding. The authenticated collector probes its owning checkout, submits the
recorded identity, and admits the project through the catalog onboarding lane.
Source publication requires the admitted scope's producer grant; enrollment
alone does not authorize reads from arbitrary paths. No matching daemon-local
checkout is required for collected code and transported Git history.

Git-history transport requires every published member of one repo-history
identity to be assigned to the same producer. Verified history sources
materialize and activate through the corpus builder.

## Configure the daemon

Create a unique 64-character lowercase hexadecimal token in an owner-only
directory and file. The path must be a real regular file with one hardlink.
For example:

```sh
install -d -m 700 ~/.config/blackbox/code-collectors
openssl rand -hex 32 > ~/.config/blackbox/code-collectors/checkout-host-a.token
chmod 600 ~/.config/blackbox/code-collectors/checkout-host-a.token
```

Add the producer once. To let agent registration claim previously unassigned
catalog scopes, use `claim_scopes = "unclaimed"`:

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
max_git_history_commits = 2000000
max_git_history_logical_bytes = 8589934592

[[code_collection.producers]]
producer_id = "checkout-host-a"
token_file = "~/.config/blackbox/code-collectors/checkout-host-a.token"
scopes = []
claim_scopes = "unclaimed"
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
- `auto_publish`: accepted for compatibility and ignored.

Publication needs no producer setting. For a project with no accepted
pointer, the first valid Ready candidate from its owning producer, on the
project's catalog scope with a non-empty full branch ref and an attached
repo-knowledge capable attachment for that scope, establishes the pointer, and
its branch ref becomes the configured ref. Every later candidate on that ref
is accepted as it finalizes. The attachment's checked-out branch does not
constrain publication. Changing the producer, ref, or scope, or rolling back,
is an operator move through `bbox_project_publisher_advance`.

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
`list` is a read and runs while the daemon is up. `revoke` requires the daemon
to be stopped: a running daemon holds the claims in memory and would write a
revoked claim back on its next persist, so `revoke` takes the offline
administration lock on the configured projects path and also checks the daemon
instance locks that cover the claims store itself. While a daemon holds either
one it returns `error.project_catalog_cli_lock`, naming the held lock, without
writing the claims store. The second check does not depend on this command and
the daemon resolving the same projects path.

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
host_label = "checkout-host-a"
service_label = "code-collector"
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

`host_label` identifies the checkout host in remote-registration errors and
defaults to the output of `hostname`, or `unknown` if that command fails.
`service_label` is optional and can identify one collector service among several
on the same host.

On every checkout-mutation cadence, after checking for a live config reload,
the collector polls the authenticated producer command channel. The poll sends
the canonical config path, host and service labels, collector version, and the
complete `enroll_roots` list, including an empty list. The daemon retains this
presence in memory and can route `bbox_project_register(path)` to a fresh
collector whose most specific root contains the path. Enrollment runs the same
scaffolding, sidecar update, and catalog onboarding procedure as the host-shell
`add` command. The response names any project files that still need to be
committed on the returned published ref.

`interval_secs` controls source collection. Queued gap and knowledge edits
poll independently at `mutation_interval_secs` (default 10 seconds, minimum
1 second), so a long source-scan interval does not delay admitted edits.
Delivery errors back off to at most 60 seconds, or the configured mutation
interval when larger. Delivery changes the checkout; publication still waits
for the configured committed ref and its publication cycle.

The collector acknowledges each delivered edit with its outcome and, for an
applied write, the SHA-256 of the bytes now at the path. The daemon settles an
applied edit only when that digest equals the edit's own content, or when the
acknowledgement claims no content; a different digest is refused with
`ack_digest_mismatch` and the edit is delivered again. A repeated
acknowledgement of an already settled edit answers `already_settled` only
after the settlement is durable in the daemon's queue, so a settlement that
failed to persist is retried by the repeat rather than reported as done.

`bbox_project_publisher_status(project_id, detail="checkout_mutations")` lists
a published project's queued edits that still need attention: pending,
applied but not yet seen in publication, failed, conflicted, or blocked behind
a predecessor. Each row carries the edit's id, path, mode, state, attempts,
last error, enqueue and acknowledgement times, and the digests of its content,
its acknowledged content and its publication base; file content is never
returned. Settled edits are counted, not listed.

The configured root must be the main Git worktree for its clone. The committed
scope at the observed `HEAD` must match the configured scope. Symlinks,
submodules, special files, `.bbox`, build output, and unsupported or oversized
files are not published.

`git_history` defaults to `false`. When enabled, the collector captures every
commit reachable from one exact `HEAD` through the stable no-follow Git
authority, refuses shallow clones, verifies the complete graph locally, and
uses resumable content-addressed upload. Multiple configured projects sharing
one Git common directory publish that repository history only once per cycle.

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
current branch in that order. Git history and published knowledge are enabled
for the enrolled entry by default. Disable individual lanes with
`--no-git-history` or `--no-published-knowledge`.

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

## Git-history ownership

A Published repository's history has one writer at a time, decided by the
journal-currency rule: the repository is transport-owned while its committed
activation journal is current, meaning the catalog materialization, the
producer grant commitment, the members' code selectors, and the selected
producer overlays all still match that journal. No marker, receipt, or proof
takes part.

- A transport-owned repository's producer overlays are served, and they are
  Git garbage-collection roots. The reindex pass takes no checkout
  Git-history lease for it, and a collected activation does not walk a
  checkout for it.
- A journal that stops being current (a grant, membership, or code selector
  change) loses its overlays at the next code activation, configuration
  reload, or daemon start, and its current ready source re-activates when the
  grant still admits it.
- A repository with no current journal is refreshed by the local lane on a
  daemon that holds checkout authority. A daemon with
  `daemon.no_checkout_authority` walks no checkout and records the
  `history_unavailable_no_attachment` history state instead.

The first start of a daemon archives any `git-transport-cutover*` or
`git-transport-checkout-parity*` file left in the state directory into
`cutover-artifacts/retired-git-transport-<timestamp>/`. Nothing reads them.

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
silently falls back to partial local data. A daemon declared without checkout
authority (`daemon.no_checkout_authority`) never cuts back: the last collected
generation stays active and the project records a structural
`no_local_attachment` cutback state.

Staleness also preserves the last good collected generation. Restore the
producer and publish again, or perform a deliberate configuration cutback.

## Producer currency

Every collector pass probes the daemon for each configured project before it
walks anything: the code lane reports the scanned HEAD, and the history lane
reports the resolved history HEAD. The daemon records, per producer and
project, the last report time and the last reported code and history HEAD, and
each collector reports its `interval_secs` on the checkout-mutation cadence.
The record is in memory, so after a restart each project awaits its first
report.

Doctor's `code_sources` section derives currency from those reports, with one
bound per producer: three times its reported interval, at least 30 minutes (a
collector that reports no interval is judged at the 120 second default).

- **Stale** (`warn`): the producer has not reported for the project within
  the bound, or has not reported since a daemon start longer ago than the
  bound. Before the bound elapses after a start, the project is reported as
  awaiting its first report (`info`).
- **Behind** (`warn`): the last reported code HEAD differs from the active
  collected generation's HEAD, or the last reported history HEAD differs from
  the served history overlay's HEAD, and no report has found it served within
  the bound. Inside the bound it is converging (`info`).
- **Current** (`info`): reported within the bound, and every reported HEAD is
  served.

Nothing warns because content is old or because the daemon cannot read a
checkout; doctor never opens one. A project with no producer assignment has no
reporter and gets no currency finding.

## Health and storage

Run `bbox_doctor` on the `ops` surface to inspect producer currency, missing
or corrupt blobs, failed activation, pending cutback, and failed retirement.
The durable store is under
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

When a retained blob is corrupt, the daemon keeps already materialized active
documents readable and requests the missing hash during the next publication.
Do not delete the store to repair one generation. Republish from the owning
checkout or complete a local cutback.
