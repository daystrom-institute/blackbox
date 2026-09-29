# blackbox

Blackbox is the daemon that keeps agent work from evaporating.

It indexes transcripts and project source, turns them into a graph agents can
walk, stores rules and conventions in one knowledge store, and runs multi-provider
work through `bro`. The point is not another search box. The point is that a
fresh agent can answer "where did this come from?", "what already decided this?",
and "which task is still alive?" without guessing from memory.

The main operator binaries are:

| Binary | Purpose |
|---|---|
| `blackboxd` | HTTP-MCP corpus daemon. One containerized workload with one state volume. |
| `blackbox` | Offline administration CLI for the project catalog and producer claims |
| `bro` | Fleet and bro execution client |
| `bro-harness` | Standalone model-turn runtime, exec'd by `fleetd` |
| `isolate` | Native deterministic harness tools |
| `fleetd` | Per-machine fleet supervisor that owns harness workers |
| `bbox-code-collector` | Checkout-host publication of project source |
| `bbox-transcript-collector` | Checkout-host publication of native session history |
| `bbox-file-collector` | Producer-host connector satellite for remote document stores |

## Docs

| Page | What it covers |
|---|---|
| [Getting Started](getting-started.md) | Build, deploy the daemon, install checkout-host satellites, connect CLIs |
| [Developing Blackbox](developing-blackbox.md) | Contributor build/test, Nix flake + isolated dev-agent world, per-worktree build isolation + sccache |
| [Operating Guide](operating-blackbox.md) | Day-2 runbooks: reindexing, re-embedding, compaction, post-update checks |
| [Internals](internals.md) | Map of the internal projections and where the deeper design pages live |
| [Graph And Retrieval Internals](graph-retrieval-internals.md) | Graph grounding, opening sequence, entity refs, edges, hybrid search ranking |
| [Index And Embedding Internals](index-embedding-internals.md) | Tantivy indexing, embedding queues, schema migration, vector and edge compaction |
| [Refactor Tools](refactor.md) | Harness-native structural refactor tooling |
| [Bro Runtime](bro-runtime.md) | Direct dispatch, resume, wait, brofiles, and provider runtime controls |
| [Knowledge Store](knowledge-store.md) | Learn, forget, render, and thread notes |
| [Design Graph](design-graph.md) | Operate this repo's `design` project graph: verbs, authority, reads, state blocks |
| [Transcript Retrieval](transcript-retrieval.md) | Search, context, sessions, messages, and freshness checks |
| [Projects And Code Indexing](projects-code-indexing.md) | Project registration, `.bbox`, code navigation, reindex, and reembed |
| [Native Transcript Collector](native-transcript-collector.md) | Publish native Claude Code and Codex session history to a corpus daemon |
| [MCP Surfaces](mcp-surfaces.md) | Named tool surfaces and operator tools |
| [Code Source Collector](code-source-collector.md) | Publish checkout-owned current files to a corpus daemon and operate source transitions |
| [Artifact Catalog](artifact-catalog.md) | Install, list, supersede, and reason about `system-defaults/` |
| [Convergence Drain Gate](converge-gate.md) | Probe live orchestration state and drain admission before converging or cycling the daemon |

## Quick links

- **Source**: [github.com/invidious9000/transcript-search](https://github.com/invidious9000/transcript-search)
- **Key paths**: `~/.local/state/blackbox/` (index, knowledge, threads), `~/.local/state/blackbox/bro/` (tasks, brofiles, MCP config)
- **Env vars**: `BBOX_PORT` (default 7264), `TRANSCRIPT_SEARCH_ROOTS`, `TRANSCRIPT_SEARCH_INDEX_PATH`
