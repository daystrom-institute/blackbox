# Blackbox project context

Blackbox provides a corpus daemon, source collectors, and agent coordination CLIs.
`blackboxd` and `bro-harness` are separate processes. Provider guidance is generated
from source knowledge; change the source and render it instead of editing generated
instructions.

Read the matching guide before that work. Paths below are repository-relative.
Skip guides unrelated to the task; do not load the whole documentation map by default.

- Understanding or changing subsystem boundaries, process contracts, or provider routing:
  `docs/project-guides/architecture.md`.
- Building, formatting, testing, or choosing a local versus cluster validation lane:
  `docs/project-guides/validation.md`.
- Operating services, configuring runtime state, or troubleshooting deployment:
  `docs/project-guides/operations.md`.
- Changing knowledge rendering, documentation, versioning, or releases:
  `docs/project-guides/authoring.md`.
- Locating additional domain documentation:
  `docs/project-guides/reference-map.md`.

Crate and subtree `AGENTS.md` files carry local invariants. Read the applicable
file when entering that area. Shared-service mutations require explicit operator
approval; inspect the service's scope before requesting it.
