# Code Source Collector

The code source collector publishes current project files and optional complete
typed Git-history snapshots from the machine that owns a checkout to the corpus
daemon. It uploads bounded raw file bytes and canonical commit facts, never Git
packs or an object database. The daemon remains responsible for chunking,
indexing, embeddings, entity references, graph snapshots, and activation.

This is an overlap facility. The daemon must already have catalog projects
whose committed `.bbox/config.toml` resolve to the configured published
scopes. Git-history transport additionally requires every published member of
one repo-history identity to be assigned to the same producer. Intake currently
stops at a verified durable `ready` source; corpus materialization and overlay
activation are the next rollout milestone.

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
```

The daemon fails closed at startup when an enabled token is unsafe, a scope is
assigned twice, or a scope does not resolve to exactly one registered project.
On SIGHUP, an invalid replacement retains the previous complete assignment and
authentication table.

## Configure the collector

Copy the same private token file to the checkout host through the operator's
secret-distribution path. Do not place it in the repository. Create a collector
configuration such as:

```toml
server_url = "https://corpus.example.invalid/"
token_file = "/home/operator/.config/blackbox/code-collectors/checkout-host-a.token"
interval_secs = 120

[[projects]]
root = "/home/operator/repos/project"
scope = { repo_id = "<recorded-repo-id>", bbox_root_relpath = "." }
git_history = true
```

The configured root must be the main Git worktree for its clone. The committed
scope at the observed `HEAD` must match the configured scope. Symlinks,
submodules, special files, `.bbox`, build output, and unsupported or oversized
files are not published.

`git_history` defaults to `false`. When enabled, the collector captures every
commit reachable from one exact `HEAD` through the stable no-follow Git
authority, refuses shallow clones, verifies the complete graph locally, and
uses resumable content-addressed upload. Multiple configured projects sharing
one Git common directory publish that repository history only once per cycle.

Publish once and wait for a terminal generation state:

```sh
bbox-code-collector --config /path/to/code-collector.toml once
```

Run continuously with bounded retry backoff:

```sh
bbox-code-collector --config /path/to/code-collector.toml run
```

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

When a retained blob is corrupt, the daemon keeps already materialized active
documents readable and requests the missing hash during the next publication.
Do not delete the store to repair one generation. Republish from the owning
checkout or complete a local cutback.
