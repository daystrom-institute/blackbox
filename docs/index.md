# blackbox

Single daemon for AI dev tooling: hybrid (BM25 + vector + path-token)
search across Claude Code / Codex / Copilot / Vibe / Gemini transcripts
**plus** registered project source code, an agentic graph projection
over the same substrate, a unified knowledge store rendered into each
provider's markdown files, work-thread tracking, and multi-provider
agent orchestration with a live multi-lane tail TUI.

The crate is `blackbox`. It produces four binaries:

| Binary | Purpose |
|---|---|
| `blackboxd` | HTTP-MCP daemon (one long-lived user service, shared across all CLIs on the host) |
| `bro` | Terminal TUI for tailing live orchestration activity |
| `bro-slack` | Slack sidecar bridge — translates Slack events into the daemon's webhook pipeline |
| `bro-irc` | LAN IRC bridge — IRC commands relayed to `bro exec/resume/status` via the daemon |

## Docs

| Page | What it covers |
|---|---|
| [Getting Started](getting-started.md) | Build, install, systemd service, connect CLIs, bootstrap |
| [Operating Guide](operating-blackbox.md) | Agentic graph internals, hybrid search, embedding pipeline, upkeep |
| [Workflow Engine](workflows.md) | Canonical reference for authoring and running workflows |
| [Rule Packets](rule-packets.md) | Compile, audit, apply — the first-match-wins classification mechanism |
| [Agent System](agent-system.md) | Installing, discovering, and dispatching registered agents |
| [Ingress Paths](ingress-paths.md) | Webhooks, pollers, crons — all three converge on the same routing pipeline |
| [Slack Bridge](slack-bridge.md) | Sidecar architecture, channel binding, triage workflow, proposal lifecycle |
| [IRC Bridge](irc-bridge.md) | LAN couch steering, ngircd setup, commands, council integration |
| [Councils & Whiteboards](councils-whiteboards.md) | Structured multi-agent deliberation (phased boards, councils) |

## Quick links

- **Source**: [github.com/invidious9000/transcript-search](https://github.com/invidious9000/transcript-search)
- **Key paths**: `~/.local/state/blackbox/` (index, knowledge, threads, notes), `~/.bro/` (tasks, teams, MCP config)
- **Env vars**: `BBOX_PORT` (default 7264), `TRANSCRIPT_SEARCH_ROOTS`, `TRANSCRIPT_SEARCH_INDEX_PATH`
