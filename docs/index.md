# blackbox

Blackbox is the daemon that keeps agent work from evaporating.

It indexes transcripts and project source, turns them into a graph agents can
walk, stores rules and decisions in one knowledge store, and runs multi-provider
work through `bro`. The point is not another search box. The point is that a
fresh agent can answer "where did this come from?", "what already decided this?",
and "which task is still alive?" without guessing from memory.

The main operator binaries are:

| Binary | Purpose |
|---|---|
| `blackbox` | Offline administration CLI. Phase 1 permits project-catalog apply only inside an explicit rehearsal root. |
| `blackboxd` | HTTP-MCP daemon. Run one long-lived user service per host. |
| `bro` | Fleet and bro execution client |
| `bro-harness` | Standalone model-turn runtime |
| `isolate` | Native deterministic harness tools |

## Docs

| Page | What it covers |
|---|---|
| [Getting Started](getting-started.md) | Build, install, systemd service, connect CLIs, bootstrap |
| [Developing Blackbox](developing-blackbox.md) | Contributor build/test, Nix flake + isolated dev-agent world, per-worktree build isolation + sccache |
| [Operating Guide](operating-blackbox.md) | Day-2 runbooks: reindexing, re-embedding, compaction, post-update checks |
| [Internals](internals.md) | Map of the internal projections and where the deeper design pages live |
| [Graph And Retrieval Internals](graph-retrieval-internals.md) | Graph grounding, opening sequence, entity refs, edges, hybrid search ranking |
| [Index And Embedding Internals](index-embedding-internals.md) | Tantivy indexing, embedding queues, schema migration, vector and edge compaction |
| [Workflow Engine](workflows.md) | Caller composition and retained workflow history |
| [Phase-Decomposer Dispatch](pd-dispatch.md) | Caller-owned phase decomposition |
| [Architecture Pathology Dispatch](pathology-dispatch.md) | Caller-owned architecture review |
| [Performance Pathology Dispatch](perf-pathology-dispatch.md) | Caller-owned performance review |
| [Reference Implementations](reference-implementations.md) | Historical application examples |
| [Atoms](atoms.md) | Retired atom execution and replacements |
| [Rule Packets](rule-packets.md) | Compile, audit, apply. First-match-wins classification. |
| [Refactor Tools And Atoms](refactor.md) | Harness-native structural refactor tooling |
| [Bro Runtime](bro-runtime.md) | Direct dispatch, resume, wait, teams, brofiles, and provider runtime controls |
| [Knowledge Store](knowledge-store.md) | Learn, decide, remember, pin, render, review, notes, and inbox |
| [Design Graph](design-graph.md) | Operate this repo's `design` project graph: verbs, authority, reads, state blocks |
| [Transcript Retrieval](transcript-retrieval.md) | Search, cite, context, sessions, messages, topics, and freshness checks |
| [Projects And Code Indexing](projects-code-indexing.md) | Project registration, `.bbox`, code navigation, reindex, and reembed |
| [Code Source Collector](code-source-collector.md) | Publish checkout-owned current files to a corpus daemon and operate source transitions |
| [Artifact Catalog](artifact-catalog.md) | Install, list, supersede, and reason about `system-defaults/` |
| [Agent System](agent-system.md) | Simple agent contracts and dispatch |
| [Badgey](badgey.md) | Retired integration and retained evidence |
| [Consultant Runtime](consultant-runtime.md) | Retired consultant runtime |
| [Project Roadmap](roadmap.md) | Generated roadmap for this repository |
| [Roadmap Tool](roadmap-tool.md) | Operator-directed prospective work tracker |
| [Ingress Paths](ingress-paths.md) | Bro control and collector transport boundaries |
| [System Events](system-events.md) | Observation journal without reaction execution |
| [Slack Bridge](slack-bridge.md) | Retired bridge and retained conversation evidence |
| [Whiteboards](whiteboards.md) | Historical evidence with preserved visibility |
| [Convergence Drain Gate](converge-gate.md) | Probe live orchestration state and drain admission before converging or cycling the daemon |

## Quick links

- **Source**: [github.com/invidious9000/transcript-search](https://github.com/invidious9000/transcript-search)
- **Key paths**: `~/.local/state/blackbox/` (index, knowledge, threads, notes), `~/.bro/` (tasks, teams, MCP config)
- **Env vars**: `BBOX_PORT` (default 7264), `TRANSCRIPT_SEARCH_ROOTS`, `TRANSCRIPT_SEARCH_INDEX_PATH`
