## Knowledge & Render Invariants

System memories are invariants and runbooks, not release ledgers or artifact
inventories. Deep or role-specific docs belong in system memories, scoped docs,
or code-owned catalogs rather than always-rendered provider memory.

`bbox_render` surfaces:

- `scope=global` patches global provider memory files and the common Blackbox
  memory file inside managed markers, with backups.
- `scope=project` writes project provider files from project-scope entries plus
  `PROJECT.md`.

Do not hand-edit managed regions. If rendered output is wrong, fix the producing
system or source memory.

Durable memories are operator-gated. If a new lesson seems worth remembering,
first check existing Blackbox memory, then present the proposed verbatim text and
wait for approval before calling `bbox_learn`, `bbox_remember`, or
`bbox_decide`. Task-local workflow notes are still allowed when the active
workflow requires them.

## Versioning & Releases

`Cargo.toml` `[package].version` is the source of truth for the Blackbox version.
Code should read it through Cargo compile-time metadata such as
`env!("CARGO_PKG_VERSION")`, not a hand-maintained constant. `Cargo.lock` should
reflect the same root package version.

This repo uses manual changelog-first releases because normal development does
not currently flow through GitHub PRs. Keep notable user-visible changes in
`CHANGELOG.md` under `Unreleased`; at release time, move them into a dated
`X.Y.Z - YYYY-MM-DD` section, commit the release metadata, create an annotated
SemVer tag (`vX.Y.Z`), and publish a GitHub Release using that changelog section
as the release body. `RELEASE.md` holds the checklist.

