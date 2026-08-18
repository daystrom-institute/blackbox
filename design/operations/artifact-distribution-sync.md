---
title: "Distribution-synced system defaults"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - operations
  - daemon-runtime
tags: [artifacts, system-defaults, drift, distribution]
brief: "Replace the manual 'install what you need' artifact stance with startup reconciliation: the daemon's distribution (container image or source checkout) carries system-defaults/, and boot advances the artifact catalog to bundled versions, version-forward, never touching operator-provenanced installs. Kills the artifact-drift class proved by the 2026-08 embed-compaction incident (the daemon ran 2026-05-30 artifacts for months; the nightly compaction arc's stale v1 packet silently no-opped for weeks)."
---

# Distribution-synced system defaults

## Problem

`system-defaults/system-defaults.md` stated the stance: "The daemon does not
auto-install this tree. Install only the defaults you want." In practice the
install set is frozen at first setup (2026-05-30 on the estate daemon) while
the repo moves: the embed-compaction arc and its policy packet drifted two
versions behind, and because v1 lacked the connectivity gate, the nightly
maintenance cron silently no-opped for weeks while a vector partition
degraded. With a handful of operators, manual upkeep is not a lane.

## Decision

The daemon's distribution carries `system-defaults/` and startup reconciles
the artifact catalog against it. The bytes travel with the binary, so a
deployed daemon can never run stale system artifacts.

- **Bundle**: the container image copies `system-defaults/` to
  `/opt/blackbox/system-defaults` at build time (the image is always built
  from the repo, so bundled defaults always match the running code). A local
  dev daemon resolves the tree from its source checkout. Config/env override:
  `BLACKBOX_SYSTEM_DEFAULTS_DIR`; empty/absent disables the sync.
- **Reconcile at startup** (`open.rs`, after `ArtifactCatalog::open`, before
  cron/workflow restore so boot restores the corrected set): walk the bundled
  tree, map leaf directories to artifact kinds (workflows, packets, crons,
  brofiles, agents, atoms, teams, mcp-surfaces), and for each artifact:
  - absent from the catalog: install it.
  - installed at a lower version with system provenance (its recorded source
    is a system-defaults path or URL, or it was installed by a prior sync):
    install the bundled version (supersession chain records the advance).
  - installed at a lower version with operator provenance, or content the
    operator modified: leave it and record a drift finding (doctor section).
  - installed at an equal or higher version: leave it.
  - bundled removals never delete; a lingering installed artifact whose name
    no longer ships is reported as drift, not removed.
- **Version-forward only.** A daemon downgraded to an older image keeps the
  newer installed artifacts and reports drift; it never silently reverts.
- **Drift surfacing**: a `bbox_doctor` section lists operator-shadowed or
  ahead-of-distribution artifacts. Sync actions and skips log at startup.

## What this does not change

- Project-scoped artifacts (`.bbox/` in checkouts) are untouched; the sync
  manages only the global system tree.
- `bbox_artifact_install` remains the lane for third-party and ad-hoc
  artifacts; provenance decides whether the sync manages them.
- Memories (`system-defaults/memories/`) have their own runtime-loading lane
  and are out of scope here.
- The human commit remains the change gate: defaults change only through
  reviewed repo commits, exactly like code.

## Implementation

1. `bbox-artifacts`: `sync_system_defaults(catalog, defaults_dir, now)` ->
   report (installed, advanced, skipped_newer, drift). Kind mapping by leaf
   directory name; unknown leaves logged and skipped.
2. Daemon: `BLACKBOX_SYSTEM_DEFAULTS_DIR` in config paths; startup reconcile
   in `open.rs` before the artifact restore; startup log lines per action.
3. Cage image: copy `system-defaults/` into the image, set the env in the
   deployment (bbox-cage).
4. Doctor: `artifact_sync` section reporting the last reconcile summary and
   any operator-shadowed artifacts.
5. Docs: `system-defaults.md` stance rewrite and
   `docs/operating-blackbox.md` install section (manual install becomes the
   exception note for third-party artifacts).

## Rejected alternatives

- **Keep manual install, add drift warnings only**: warnings without
  enforcement are what the estate already had in effect; nobody acts on them.
- **Sync from the served publication transport** (defaults ride the knowledge
  publication): works on the cage but couples artifact truth to a specific
  project's collector health; bundling is simpler and deployment-atomic.
- **Auto-remove unshipped artifacts**: removal is an operator decision; the
  sync reports instead.
