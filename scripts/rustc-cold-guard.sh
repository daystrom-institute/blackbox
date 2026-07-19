#!/bin/sh
# Cold-checkout build guard, wired as the cargo rustc-wrapper via
# .cargo/config.toml. Refuses to compile in a COLD LINKED WORKTREE on the
# macOS control-plane host, where a first build is a 20+ minute
# full-dependency compile plus syspolicyd assessment of every fresh binary.
# Warmth is the discriminator, not task size (PROJECT.md, Where Heavy Work
# Runs).
#
# Passthrough (exec "$@", zero behavior change) whenever ANY of:
#   - BBOX_ALLOW_COLD_BUILD=1 (deliberate operator override)
#   - not macOS (lane pods and CI are Linux)
#   - the checkout is a base repo (.git is a directory, not a gitfile)
#   - the worktree is warm (any .rlib already present under target/)
#
# Env precedence makes this self-disabling where it should be: daemon
# dispatches and lane pods set RUSTC_WRAPPER in the environment
# (fleet.json project_dispatch.env / the pod template), and env beats
# cargo config, so this wrapper never runs there.

[ "${BBOX_ALLOW_COLD_BUILD:-}" = "1" ] && exec "$@"
[ "$(uname -s)" = "Darwin" ] || exec "$@"

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(dirname -- "$script_dir")

# Only linked worktrees are guarded; a base checkout has a .git directory.
[ -f "$root/.git" ] || exec "$@"

if [ -d "$root/target" ] \
    && [ -n "$(find "$root/target" -name '*.rlib' -print -quit 2>/dev/null)" ]; then
    exec "$@"
fi

cat >&2 <<EOF
blackbox cold-checkout guard: refusing to compile in a cold linked worktree
on the macOS control-plane host ($root).

A cold build here is a 20+ minute full-dependency compile plus per-binary
macOS assessment, stolen from every sibling agent lane. Options:

  - Gates run lane-side against a pushed ref:
      ~/repos/bbox-cage/build/lanes/lane-pool.sh claim   (warm lane, seconds)
      ~/repos/bbox-cage/build/submit-bbox-verify.sh --ref <ref>
  - Dispatched edit-only work: done-criteria is committed work with tests
    WRITTEN, not locally-run gates; the orchestrator verifies lane-side.
    See prompts/agents/edit-only-worktree.md.
  - Deliberately warming THIS worktree is an operator decision:
      BBOX_ALLOW_COLD_BUILD=1 cargo ...

Full contract: PROJECT.md, "Where Heavy Work Runs".
EOF
exit 1
