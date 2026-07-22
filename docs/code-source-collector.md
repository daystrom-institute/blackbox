# Code Source Collector

The code source collector publishes current project files from the machine that
owns a checkout to the corpus daemon. The collector only walks, hashes, and
uploads bounded raw bytes. The daemon remains responsible for chunking,
indexing, embeddings, entity references, and graph snapshots.

This is an overlap facility. The daemon must already have exactly one local
project registration whose committed `.bbox/config.toml` resolves to the same
published scope. Git history still comes from that registered checkout.

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
max_manifest_files = 250000
max_manifest_logical_bytes = 5368709120
max_open_uploads_per_producer = 2
retained_generations = 2
unreferenced_blob_grace_hours = 168
stale_warning_hours = 24

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
```

The configured root must be the main Git worktree for its clone. The committed
scope at the observed `HEAD` must match the configured scope. Symlinks,
submodules, special files, `.bbox`, build output, and unsupported or oversized
files are not published.

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

When a retained blob is corrupt, the daemon keeps already materialized active
documents readable and requests the missing hash during the next publication.
Do not delete the store to repair one generation. Republish from the owning
checkout or complete a local cutback.
