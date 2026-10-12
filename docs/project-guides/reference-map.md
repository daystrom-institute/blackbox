## Docs Map

Use `PROJECT.md` as the map and guardrail layer. Put detailed procedures in docs
or agent skills and link/pointer from here.

- `README.md` - user-facing overview and setup.
- `docs/index.md` - human documentation map.
- `examples/runnable-examples.md`, `system-defaults/system-defaults.md` - maps for
  tutorial examples and installable default artifacts.
- `prompts/README.md` - map of checked-in prose prompts (operator-pointed
  interactive prompts and dispatched-agent lenses). Distinct from
  `system-defaults/` artifacts and agent skills.
- `docs/agent-skills.md` - the `bbox-*` agent skills: global ones under
  `system-defaults/skills/` installed by `scripts/sync-skills.sh`, project ones
  under `.agents/skills/`.
- `docs/getting-started.md`, `docs/operating-blackbox.md`,
  `docs/operations.md` - operational setup and day-2 runbooks.
- `docs/operations-isolated-dev-daemon.md` - running a lightweight throwaway
  blackboxd for live validation without touching prod state.
- `docs/internals.md`, `docs/index-embedding-internals.md`,
  `docs/graph-retrieval-internals.md` - architecture internals.
- `docs/transcript-retrieval.md`, `docs/knowledge-store.md`,
  `docs/projects-code-indexing.md` - core corpus surfaces.
- `system-defaults/memories/system-memory-catalog.md` - Obsidian navigation
  map for system memories; not loaded as a runtime memory.
- `docs/refactor.md` - source editing through native provider tools.

- `design/design-corpus.md` - Obsidian-friendly map for the design corpus.
- `research/research-corpus.md` - map for the research corpus: a point-in-time,
  evidence-graded study of the external problem space (reference harnesses,
  provider APIs, protocols) that feeds `design/`. Sibling of `design/`, distinct
  `corpus: blackbox-research`. First track + charter:
  `research/harness/harness-tracks.md`.
- `specs/specs-corpus.md` - map for the specs corpus: the CANON — normative,
  source-grounded contracts for what each subsystem should be/do. Third sibling
  of `design/` (intent) and `research/` (description), distinct
  `corpus: blackbox-spec`; backfilled by inverting code + design + research.
- `design/list-design-docs.sh` - list design docs whose frontmatter lifecycle
  is `proposed` or `partial`.
- `design/connectors/` - topic home for remote-source connectors:
  producer-plane observers of remote document stores and API datasets
  (Google Drive, OneDrive/SharePoint, Xero, Slack) publishing into the
  corpus over the collector-style transport.
- `design/corpus/` - topic home for agentic corpus, knowledge/memory, notes,
  storage, and code navigation designs.
- `design/orchestration/` - topic home for atoms, agents, workflows,
  supervision, phase decomposition, runtime allocation, and live handoff
  designs.

- `design/daemon-runtime/control-plane.md` - headless task control, roster
  observation and managed-worktree closeout.
- `design/daemon-runtime/` - topic home for blackboxd's execution
  architecture: tokio topology, plane isolation, lock discipline, and
  persistence actors.

- `design/integrations/` - topic home for editor/chat/external UI
  integrations such as Obsidian and Slack.
- `design/surfaces/` - topic home for MCP surfaces, workspace tools, and
  provider transcript read planes.
- `design/operations/` - topic home for config/artifact lifecycle, bundles,
  and doctor.
- Legacy lifecycle folders such as `design/archive/`, `design/proposed/`, and
  `design/partial/` may appear in old checkouts. Prefer frontmatter
  `lifecycle` over path when determining currentness, and verify against code
  before treating any design as current behavior.
