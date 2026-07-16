# Corpus-host container images

Linux container images for the two services that run on the operator's k3s
cluster ("the cage") per slice 0 of
`design/daemon-runtime/remote-corpus-host.md`:

- **blackboxd (corpus role)** on port **7264** - the corpus authority (tantivy,
  HNSW vectors, EdgeIndex, knowledge/threads/notes stores).
- **blackopsd** on port **7266** - the operational singleton (agent graph,
  mailboxes, workflows, schedules).

Both are built for **linux/amd64** (the cage's x86 workers) from a single
shared builder stage in `Dockerfile`, so the workspace compiles once.

## Files

| File | Purpose |
|---|---|
| `Dockerfile` | Multi-stage: one `builder`, two runtime targets (`blackboxd`, `blackopsd`). |
| `Dockerfile.dockerignore` | Trims the build context (root-relative; honored by BuildKit with `-f`). |
| `build.sh` | buildx driver; parameterized registry + git-derived tags. |

## Building

The registry is **operator-chosen and never hardcoded**. Set `REGISTRY` to push;
leave it unset for local-only tags.

```bash
# Build both, load into the local docker store (no push):
deploy/docker/build.sh

# Build one target:
deploy/docker/build.sh blackboxd

# Build and push to your homelab registry:
REGISTRY=registry.lan:5000 deploy/docker/build.sh --push
```

Tags are `${IMAGE_NS}-${target}:${BUILD_ID}` (default `IMAGE_NS=blackbox`), e.g.
`blackbox-blackboxd:v1.2.3-4-gabc1234`. `BUILD_ID` defaults to
`git describe --tags --always --dirty`, falling back to the 12-char short SHA and
then a timestamp, matching the `BLACKBOX_BUILD_ID` convention in `build.rs`.

### Fast path vs emulated path

The images target **linux/amd64**. How fast the build is depends on the host:

- **Fast (native):** build on a **linux x86_64 box** or the cluster's own runner
  infrastructure, where `linux/amd64` is native. This is the recommended path
  for CI/release builds. A full workspace release build (including the one-time
  V8 prebuilt download and link) is heavy but runs at native speed.
- **Slow (emulated):** building `linux/amd64` from an **arm64 macOS host** works
  but runs the entire Rust compile under QEMU emulation, which is dramatically
  slower (hours, not minutes, for a cold build) and memory-hungry. Use it only
  for a one-off local check, not routine builds. First register QEMU binfmt:
  ```bash
  docker run --privileged --rm tonistiigi/binfmt --install amd64
  ```
  To iterate on the Dockerfile/build logic locally without emulation, build for
  your **native** arch instead and treat amd64 as identical modulo the target
  triple:
  ```bash
  PLATFORM=linux/arm64 deploy/docker/build.sh
  ```

### rusty_v8 / V8 note

The daemon links `bro-harness -> bro-code-mode -> v8` (rusty_v8). On the default
build path the `v8` crate's build script **downloads a prebuilt static
`librusty_v8`** for the target from the denoland GitHub releases; it does **not**
compile V8 from source, so the builder needs no depot_tools/gn/ninja, only
outbound HTTPS at build time. The prebuilt is published for
`x86_64-unknown-linux-gnu` at the pinned crate version. Knobs:

- `RUSTY_V8_MIRROR` - alternate releases base URL (air-gapped mirror).
- `RUSTY_V8_ARCHIVE` - exact prebuilt archive path/URL.
- `V8_FROM_SOURCE=1` - force a full source build (heavy; avoid).

The build caches the prebuilt under `$CARGO_HOME/.rusty_v8`; the Dockerfile's
cargo cache mounts make repeat builds skip both the crates.io fetch and the V8
download.

Similarly, `tree-sitter-language-pack`'s build script downloads its parser
sources tarball from GitHub at build time (`TSLP_SOURCE_BUNDLE_URL` /
`TSLP_OFFLINE` are its mirror/offline knobs), so the builder stage needs
outbound HTTPS in general, not only for V8.

Disk note: a cold builder run writes roughly 15-20 GB across the BuildKit cache
mounts (cargo registry, target dir) plus image layers. On a VM-backed docker
host (colima, Docker Desktop) size the VM disk accordingly or the build dies
mid-compile with `No space left on device`.

## The k8s consumption contract

The Deployment/StatefulSet manifests live in the **operator's infra repo**, not
here. This is the contract those manifests must satisfy.

### Volumes (state)

Mount a writable PVC (Longhorn) per service. Chown it to uid/gid **10001** (the
image's `blackbox` user) or set `fsGroup: 10001`.

| Service | Mount point | State env |
|---|---|---|
| blackboxd | `/var/lib/blackbox` | `BLACKBOX_STATE_DIR=/var/lib/blackbox/state` (image default). Derived stores (`BLACKBOX_KNOWLEDGE_PATH`, `BLACKBOX_THREADS_PATH`, `TRANSCRIPT_SEARCH_INDEX_PATH`, ...) default under it; override individually only if you split volumes. |
| blackopsd | `/var/lib/blackopsd` | `BLACKOPSD_STATE_DIR=/var/lib/blackopsd/state` (image default). |

### Service token (mounted Secret)

Both services read the **same** env `BLACKBOX_SERVICE_TOKEN_FILE`. The same
`service.token` bearer value is provisioned to the cluster as a Secret through
the homelab's established secret-sourcing path (never committed).

`bro-rpc`'s `validate_private_file` (`crates/bro-rpc/src/auth.rs`) enforces:

- a **real file, not a symlink**,
- owner uid == the process euid (10001),
- **nlink == 1**,
- mode & `0o077 == 0` (i.e. **0600 or 0400**, no group/other bits).

Two k8s footguns:

1. **A Secret volume mount is a symlink farm** (`service.token -> ..data/...`);
   `validate_private_file` rejects symlinks. Do **not** point
   `BLACKBOX_SERVICE_TOKEN_FILE` straight at a Secret volume mount. Instead
   stage the Secret elsewhere and copy it to a real `0600` file owned by 10001
   on the writable state volume in an initContainer (or the entrypoint), then
   point the env at the copy. A `subPath` Secret mount produces a real file but
   never receives updates - acceptable only if the token is static.
2. Set the Secret volume **`defaultMode: 0400`** (or `0600`) and
   `runAsUser: 10001` / `fsGroup: 10001` so the staged file has the right owner
   and no group/other bits.

The image defaults point the env at `.../service.token` on the state volume -
the reconciled real-file location, not a Secret mount.

### Non-loopback bind (opt-in, pending)

Both services **fail closed on a non-loopback bind today**:

- blackboxd corpus role: `src/server/run.rs`.
- blackopsd: `crates/blackopsd/src/config.rs::validate`.

A parallel change adds an explicit **non-loopback opt-in** (config field + env
override) for containerized deployment. Once it lands, the Deployment sets, per
service:

```
# blackboxd
BBOX_BIND=0.0.0.0
<NON_LOOPBACK_OPT_IN_ENV>=1    # placeholder: use the real knob name

# blackopsd
BLACKOPSD_BIND=0.0.0.0:7266
<NON_LOOPBACK_OPT_IN_ENV>=1    # placeholder: use the real knob name
```

The images deliberately default to the **safe loopback bind** so the current
binary boots; the manifest overrides the bind together with the opt-in. If
blackopsd talks to the corpus service across a pod boundary, its `blackboxd_url`
validator (loopback-only today) needs the same opt-in. SSH tunnels claiming the
freed loopback ports remain a supported alternative that keeps every URL
loopback (see the design doc's transport section).

### Health probes

`/healthz` and `/readyz` exist on both services. Wire:

```yaml
livenessProbe:  { httpGet: { path: /healthz, port: 7264 } }   # 7266 for blackopsd
readinessProbe: { httpGet: { path: /readyz,  port: 7264 } }   # 7266 for blackopsd
```

The image also ships a container-local `HEALTHCHECK` that probes loopback, which
works regardless of the external bind; k8s ignores it and uses the probes above.

### Registry / push

Images are pushed to the **operator-chosen registry** (`REGISTRY` in
`build.sh`). Nothing here pushes anywhere by default. The Flux/Pulumi manifests
in the infra repo reference whatever `REGISTRY`/tag you built and pushed.
