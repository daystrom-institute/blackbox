# blackbox

Blackbox is the daemon that keeps agent work from evaporating.

It indexes transcripts and project source, turns them into a graph agents can
walk, stores rules and decisions in one knowledge store, and runs multi-provider
work through `bro`. The point is not another search box. The point is that a
fresh agent can answer "where did this come from?", "what already decided this?",
and "which task is still alive?" without guessing from memory.

The crate is `blackbox`. It produces four binaries:

| Binary | Purpose |
|---|---|
| `blackboxd` | HTTP-MCP daemon. Run one long-lived user service per host. |
| `bro` | Terminal TUI for tailing live orchestration activity |
| `bro-slack` | Slack sidecar bridge. Translates Slack events into the daemon's webhook pipeline. |
| `bro-irc` | LAN IRC bridge. Relays IRC commands to `bro exec/resume/status`. |

## Docs

| Page | What it covers |
|---|---|
| [Getting Started](getting-started.md) | Build, install, systemd service, connect CLIs, bootstrap |
| [Operating Guide](operating-blackbox.md) | What landed in the agentic corpus and how to keep it healthy |
| [Workflow Engine](workflows.md) | Canonical reference for authoring and running workflows |
| [Atoms](atoms.md) | Install, discover, invoke, resume, and bind reusable capabilities |
| [Rule Packets](rule-packets.md) | Compile, audit, apply. First-match-wins classification. |
| [Refactor Tools And Atoms](refactor.md) | Structural refactor primitives plus shipped Java/Rust refactor atoms |
| [Bro Runtime](bro-runtime.md) | Direct dispatch, resume, wait, teams, brofiles, and provider runtime controls |
| [Knowledge Store](knowledge-store.md) | Learn, decide, remember, pin, render, review, notes, and inbox |
| [Transcript Retrieval](transcript-retrieval.md) | Search, cite, context, sessions, messages, topics, and freshness checks |
| [Projects And Code Indexing](projects-code-indexing.md) | Project registration, `.bbox`, code navigation, reindex, and reembed |
| [Artifact Catalog](artifact-catalog.md) | Install, list, supersede, and reason about `system-defaults/` |
| [Agent System](agent-system.md) | Legacy registered-agent compatibility surface |
| [Ingress Paths](ingress-paths.md) | Webhooks, pollers, and crons. One routing pipeline. |
| [Slack Bridge](slack-bridge.md) | Sidecar architecture, channel binding, triage workflow, proposal lifecycle |
| [IRC Bridge](irc-bridge.md) | LAN couch steering, ngircd setup, commands, council integration |
| [Councils & Whiteboards](councils-whiteboards.md) | Structured multi-agent deliberation (phased boards, councils) |

## Quick links

- **Source**: [github.com/invidious9000/transcript-search](https://github.com/invidious9000/transcript-search)
- **Key paths**: `~/.local/state/blackbox/` (index, knowledge, threads, notes), `~/.bro/` (tasks, teams, MCP config)
- **Env vars**: `BBOX_PORT` (default 7264), `TRANSCRIPT_SEARCH_ROOTS`, `TRANSCRIPT_SEARCH_INDEX_PATH`
