# syntax=docker/dockerfile:1.7
#
# Blackbox corpus-host container images (slice 0 of
# design/daemon-runtime/remote-corpus-host.md).
#
# One shared builder stage compiles the workspace release binaries once; the two
# runtime targets (blackboxd, blackopsd) copy from it so the heavy compile cost
# is paid a single time per build. Build a specific service with:
#
#     docker build --target blackboxd ...
#     docker build --target blackopsd ...
#
# BuildKit reuses the cached `builder` layer across both target invocations, so
# the second build only re-runs the small runtime-stage copy. See build.sh.
#
# Target platform: linux/amd64 (the cage's x86 workers). Building this on an
# arm64 host requires QEMU emulation and is slow; see README.md for the
# native-vs-emulated tradeoff.

# ---------------------------------------------------------------------------
# Builder: compile blackboxd (root `blackbox` crate) and blackopsd
# (crates/blackopsd) for the native container target in one pass.
# ---------------------------------------------------------------------------
#
# Debian trixie is chosen deliberately so the builder glibc matches the
# trixie-slim runtime glibc (2.41). Trixie (not bookworm) is load-bearing for
# git at RUNTIME: the provenance exporter uses `git notes append --separator`
# (git >= 2.44), bookworm ships 2.39, and bookworm-backports does not carry
# git. Rust must be >= 1.88 for the let-chains and Rust 2024 edition this
# workspace uses; `rust:1-trixie` floats to the latest stable 1.x to avoid
# staleness. Pin to an exact `rust:1.NN-trixie` tag if you need
# bit-reproducible builds.
FROM rust:1-trixie AS builder

# The daemon links bro-harness -> bro-code-mode -> the `v8` crate (rusty_v8),
# whose build script downloads a PREBUILT static librusty_v8 per target from
# https://github.com/denoland/rusty_v8/releases (confirmed available for
# x86_64-unknown-linux-gnu at the pinned v8 crate version). It does NOT compile
# V8 from source on the default path, so the builder does not need depot_tools,
# gn, or ninja. Relevant env knobs (all optional):
#   RUSTY_V8_MIRROR  - override the GitHub releases base URL (air-gapped mirror)
#   RUSTY_V8_ARCHIVE - point at an exact local/remote prebuilt .a[.gz] archive
#   V8_FROM_SOURCE=1 - force a full V8 source build (heavy; avoid)
# The download needs outbound HTTPS at build time; keep the build network-on.
# (tree-sitter-language-pack's build script likewise downloads its parser
# sources tarball from GitHub at build time; TSLP_SOURCE_BUNDLE_URL /
# TSLP_OFFLINE are its mirror/offline knobs.)
#
# The v8 build script caches the prebuilt under $CARGO_HOME/.rusty_v8; the cache
# mounts below make repeat builds skip both the crates.io fetch and the V8
# download.

# git is needed at BUILD time by build.rs (git rev-parse for BLACKBOX_BUILD_ID);
# it is present in the rust image. pkg-config/libssl are not required: the
# workspace uses rustls, not openssl (reqwest is rustls-tls).
WORKDIR /src

# Copy the whole workspace. A .dockerignore keeps target/ and VCS cruft out of
# the build context so the context upload stays small.
COPY . .

# Build both release binaries in a single invocation so the shared dependency
# graph (including the one-time V8 download + link) compiles once.
# Cache mounts: registry + git for crates.io, and the workspace target dir.
# NOTE: the target cache is keyed per-build; the release binaries are copied out
# to /out below because a cache-mounted target/ is not present in later stages.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release \
        -p blackbox --bin blackboxd \
        -p blackopsd --bin blackopsd \
    && mkdir -p /out \
    && cp target/release/blackboxd /out/blackboxd \
    && cp target/release/blackopsd /out/blackopsd

# ---------------------------------------------------------------------------
# Shared runtime base: slim Debian with the runtime deps both services need.
# ---------------------------------------------------------------------------
#
# Debian-slim (not distroless) is required because the corpus indexer and the
# provenance exporter shell out to `git` at RUNTIME:
#   - crates/bbox-corpus-core/src/git.rs (git notes append/config, git log)
#   - crates/bbox-corpus-index/src/index/git_history.rs (commit indexing)
#   - crates/bbox-corpus-index/src/index/project_files.rs (git ls-files)
# so `git` must be on PATH in the image, not just at build time.
#
# ca-certificates is required for the outbound HTTPS call to the Voyage
# embedding API (and any other TLS egress). Both binaries use rustls, which
# reads the system trust store via rustls-native-certs.
# trixie-slim, not bookworm-slim: the provenance exporter uses
# `git notes append --separator`, which needs git >= 2.44. Bookworm ships
# 2.39 (caught failing at runtime by the corpus-core git-notes round-trip
# test on the first Linux CI run) and bookworm-backports does not carry git;
# trixie clears 2.44 natively. Must stay in glibc lockstep with the builder
# stage above (trixie = 2.41 on both sides).
FROM debian:trixie-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user. Fixed uid/gid so mounted volume ownership is
# predictable in the k8s manifests (chown the PVC / set fsGroup to 10001).
RUN groupadd --gid 10001 blackbox \
    && useradd --uid 10001 --gid 10001 --home-dir /home/blackbox --create-home blackbox

# ---------------------------------------------------------------------------
# blackboxd (corpus role) runtime image. Port 7264.
# ---------------------------------------------------------------------------
FROM runtime-base AS blackboxd
COPY --from=builder /out/blackboxd /usr/local/bin/blackboxd

# System memory defaults are REQUIRED at boot. Resolution (crates/bbox-config/
# src/config.rs) is BLACKBOX_DEFAULTS_DIR, then config [paths].defaults_dir,
# then <exe_dir>/../share/blackbox/memories (installed layout), then the
# compile-time dev fallback (which points at /src inside the builder and does
# not exist here). Ship the defaults at the installed-layout path so tier 3
# resolves; without this the daemon exits at startup with "system memory
# defaults directory is empty or missing".
COPY --from=builder /src/system-defaults/memories /usr/local/share/blackbox/memories

# HOME is where dirs::home_dir() resolves; keep it writable. State lives on a
# mounted volume; point every store env at it. Mount a writable PVC at
# /var/lib/blackbox in the k8s manifest. The vector store derives
# <BLACKBOX_STATE_DIR>/vectors (BLACKBOX_VECTORS_DIR overrides;
# gap-47f167de); the tantivy index still needs an explicit
# TRANSCRIPT_SEARCH_INDEX_PATH pin in the manifest (see README).
ENV HOME=/home/blackbox \
    BLACKBOX_STATE_DIR=/var/lib/blackbox/state \
    BLACKBOX_RUNTIME_ROLE=corpus \
    BBOX_PORT=7264
#
# Non-loopback bind: the corpus role fails closed on a non-loopback bind today
# (src/server/run.rs). A parallel change is adding an explicit non-loopback
# opt-in (config field + env override) for containerized deployment. Once it
# lands, the k8s Deployment sets BOTH of:
#     BBOX_BIND=0.0.0.0
#     <NON_LOOPBACK_OPT_IN_ENV>=1   # placeholder: replace with the real knob
# Do NOT bake BBOX_BIND=0.0.0.0 into the image: the current binary would refuse
# to start. The image defaults to the safe loopback bind; the deployment
# overrides it together with the opt-in.

# Service token arrives as a mounted k8s Secret. bro-rpc's validate_private_file
# (crates/bro-rpc/src/auth.rs) demands: a REAL file (not a symlink), owner ==
# the process euid, nlink == 1, and mode & 0o077 == 0 (i.e. 0600/0400, no group
# or other bits). Two k8s footguns follow from that:
#   1. A Secret volume mount is a SYMLINK farm (service.token -> ..data/...);
#      validate_private_file rejects symlinks outright. Mount the secret to a
#      staging path and copy it to a real 0600 file on the writable volume in an
#      initContainer / entrypoint, then point BLACKBOX_SERVICE_TOKEN_FILE at the
#      copy. A `subPath` secret mount yields a real file but never updates.
#   2. Set the Secret volume defaultMode to 0400 (or 0600) and fsGroup/runAsUser
#      to 10001 so the file is owned by this user with no group/other bits.
# BLACKBOX_SERVICE_TOKEN_FILE is read by src/server/open.rs; default would be
# $BLACKBOX_STATE_DIR/service.token. Point it at the reconciled real file:
ENV BLACKBOX_SERVICE_TOKEN_FILE=/var/lib/blackbox/state/service.token

USER blackbox
WORKDIR /var/lib/blackbox
VOLUME ["/var/lib/blackbox"]
EXPOSE 7264

# Health/readiness endpoints exist at /healthz and /readyz (src/server/mcp.rs).
# k8s should wire livenessProbe -> /healthz and readinessProbe -> /readyz on
# port 7264. The container-local HEALTHCHECK below hits loopback, which works
# regardless of the external bind; it uses bash's /dev/tcp so the image does
# not need curl/wget.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/7264 && printf "GET /healthz HTTP/1.0\r\n\r\n" >&3 && grep -q "200 OK" <&3' || exit 1

ENTRYPOINT ["/usr/local/bin/blackboxd"]

# ---------------------------------------------------------------------------
# blackopsd runtime image. Port 7266.
# ---------------------------------------------------------------------------
FROM runtime-base AS blackopsd
COPY --from=builder /out/blackopsd /usr/local/bin/blackopsd

ENV HOME=/home/blackbox \
    BLACKOPSD_STATE_DIR=/var/lib/blackopsd/state
#
# blackopsd's bind guard (crates/blackopsd/src/config.rs::validate) also fails
# closed on a non-loopback bind today ("must bind a loopback address"). The same
# parallel opt-in applies. Once it lands, the k8s Deployment sets:
#     BLACKOPSD_BIND=0.0.0.0:7266
#     <NON_LOOPBACK_OPT_IN_ENV>=1   # placeholder: replace with the real knob
# and, if blackopsd points at the corpus service across the pod boundary, its
# blackboxd_url validator (currently loopback-only) needs the same opt-in.
# Image default stays loopback so the current binary boots.

# Same service-token contract as blackboxd. blackopsd reads the SAME env,
# BLACKBOX_SERVICE_TOKEN_FILE (crates/blackopsd/src/config.rs), default
# $BLACKOPSD_STATE_DIR/../service.token. The k8s Secret symlink / ownership /
# defaultMode 0400|0600 caveats above apply identically here.
ENV BLACKBOX_SERVICE_TOKEN_FILE=/var/lib/blackopsd/service.token

USER blackbox
WORKDIR /var/lib/blackopsd
VOLUME ["/var/lib/blackopsd"]
EXPOSE 7266

# /healthz and /readyz exist (crates/blackopsd/src/service.rs). Same probe wiring.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/7266 && printf "GET /healthz HTTP/1.0\r\n\r\n" >&3 && grep -q "200 OK" <&3' || exit 1

ENTRYPOINT ["/usr/local/bin/blackopsd"]
