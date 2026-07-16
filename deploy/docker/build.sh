#!/usr/bin/env bash
#
# Build the Blackbox corpus-host images for the cage (linux/amd64 x86 workers).
#
# Both services share one builder stage in deploy/docker/Dockerfile, so the
# workspace compiles once; each `--target` invocation reuses the cached builder
# layer under BuildKit.
#
# The registry is OPERATOR-CHOSEN and never hardcoded. Set REGISTRY to your
# homelab registry (e.g. registry.lan:5000/blackbox) to tag/push there. With no
# REGISTRY the images are tagged locally only and never pushed.
#
# Usage:
#   deploy/docker/build.sh                      # build both, load locally
#   REGISTRY=registry.lan:5000 deploy/docker/build.sh --push
#   PLATFORM=linux/arm64 deploy/docker/build.sh # native build on an arm64 box
#   deploy/docker/build.sh blackboxd            # build one target
#
# Env:
#   REGISTRY   registry prefix; empty => local tags only (implies no --push)
#   PLATFORM   target platform (default linux/amd64)
#   IMAGE_NS   image namespace/basename prefix (default: blackbox)
#   BUILD_ID   override the build stamp (default: git describe/short sha)
#
# Flags:
#   --push     push to $REGISTRY (requires REGISTRY set)
#   --load     load into the local docker image store (default when not pushing;
#              only valid for a single platform)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DOCKERFILE="${REPO_ROOT}/deploy/docker/Dockerfile"

REGISTRY="${REGISTRY:-}"
PLATFORM="${PLATFORM:-linux/amd64}"
IMAGE_NS="${IMAGE_NS:-blackbox}"

# Build id consistent with build.rs (git rev-parse --short=12 HEAD), extended
# with `git describe` when tags exist so pushed images are traceable to a
# release. Falls back to a timestamp when git is unavailable, matching build.rs.
if [[ -z "${BUILD_ID:-}" ]]; then
  if BUILD_ID="$(git -C "${REPO_ROOT}" describe --tags --always --dirty 2>/dev/null)"; then
    :
  else
    BUILD_ID="$(git -C "${REPO_ROOT}" rev-parse --short=12 HEAD 2>/dev/null || date +%s)"
  fi
fi

PUSH=0
LOAD=0
TARGETS=()
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --load) LOAD=1 ;;
    blackboxd|blackopsd) TARGETS+=("$arg") ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done
if [[ ${#TARGETS[@]} -eq 0 ]]; then
  TARGETS=(blackboxd blackopsd)
fi

if [[ "$PUSH" -eq 1 && -z "$REGISTRY" ]]; then
  echo "--push requires REGISTRY to be set" >&2
  exit 2
fi
# Default to --load when neither push nor load requested and it is a single
# platform (buildx can only --load one platform at a time).
if [[ "$PUSH" -eq 0 && "$LOAD" -eq 0 && "$PLATFORM" != *,* ]]; then
  LOAD=1
fi

# Prefer buildx (required for cross-platform emulation and multi-target caching).
# Fall back to the BuildKit-backed classic builder if the buildx plugin is
# missing; note cross-arch emulation still needs QEMU binfmt registered
# (`docker run --privileged --rm tonistiigi/binfmt --install amd64`).
if docker buildx version >/dev/null 2>&1; then
  ENGINE=buildx
else
  ENGINE=classic
  echo "note: docker buildx plugin not found; using DOCKER_BUILDKIT classic builder." >&2
  echo "      cross-arch (${PLATFORM}) builds need QEMU binfmt:" >&2
  echo "      docker run --privileged --rm tonistiigi/binfmt --install amd64" >&2
fi

tag_for() {
  local target="$1"
  local base="${IMAGE_NS}-${target}"   # e.g. blackbox-blackboxd
  if [[ -n "$REGISTRY" ]]; then
    echo "${REGISTRY%/}/${base}:${BUILD_ID}"
  else
    echo "${base}:${BUILD_ID}"
  fi
}

for target in "${TARGETS[@]}"; do
  tag="$(tag_for "$target")"
  echo ">>> building ${target} -> ${tag} (platform=${PLATFORM}, build_id=${BUILD_ID})"

  common_args=(
    --file "${DOCKERFILE}"
    --target "${target}"
    --platform "${PLATFORM}"
    --tag "${tag}"
    --build-arg "BUILDKIT_INLINE_CACHE=1"
  )

  if [[ "$ENGINE" == "buildx" ]]; then
    out_flag=()
    [[ "$PUSH" -eq 1 ]] && out_flag=(--push)
    [[ "$LOAD" -eq 1 ]] && out_flag=(--load)
    docker buildx build "${common_args[@]}" "${out_flag[@]}" "${REPO_ROOT}"
  else
    DOCKER_BUILDKIT=1 docker build "${common_args[@]}" "${REPO_ROOT}"
    if [[ "$PUSH" -eq 1 ]]; then
      docker push "${tag}"
    fi
  fi
done

echo ">>> done. images:"
for target in "${TARGETS[@]}"; do
  echo "    $(tag_for "$target")"
done
