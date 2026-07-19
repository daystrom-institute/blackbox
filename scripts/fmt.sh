#!/usr/bin/env bash
# One formatter entry point for local macOS and cluster-backed Linux work.
set -euo pipefail

readonly RUSTFMT_TOOLCHAIN="1.96.0"

if ! cargo "+${RUSTFMT_TOOLCHAIN}" fmt --version >/dev/null 2>&1; then
    cat >&2 <<EOF
scripts/fmt.sh: rustfmt is unavailable for pinned toolchain ${RUSTFMT_TOOLCHAIN}.

Local remedy:
  rustup toolchain install ${RUSTFMT_TOOLCHAIN} --profile minimal --component rustfmt

Lane remedy:
  the blackbox builder image is stale; release this lane and claim one
  provisioned with the current bbox-cage image.
EOF
    exit 2
fi

exec cargo "+${RUSTFMT_TOOLCHAIN}" fmt "$@"
