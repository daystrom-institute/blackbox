# blackbox

Single daemon for AI dev tooling: hybrid (BM25 + vector + path-token)
search across Claude Code / Codex / Copilot / Vibe / Gemini transcripts
plus registered project source code, an agentic graph projection over the
same substrate, a unified knowledge store rendered into each provider's
markdown files, work-thread tracking, and multi-provider agent
orchestration with a live multi-lane tail TUI. Backed by
[tantivy](https://github.com/quickwit-oss/tantivy) (Rust) and HNSW
vector partitions.

The crate is `blackbox`. It produces four binaries:

| Binary | Purpose |
|---|---|
| `blackboxd` | HTTP-MCP daemon. One long-lived user service per host. |
| `bro` | Terminal TUI for tailing live orchestration activity |
| `bro-slack` | Slack sidecar bridge. Translates Slack events into the daemon's webhook pipeline. |
| `bro-irc` | LAN IRC bridge. Relays IRC commands to `bro exec/resume/status`. |

## Documentation

Full docs at [`docs/index.md`](docs/index.md). Key entry points:

| Page | Covers |
|---|---|
| [Getting Started](docs/getting-started.md) | Build, install, systemd, connect CLIs, bootstrap |
| [Operations](docs/operations.md) | Config, backup, upkeep, disk layout |
| [Operating Guide](docs/operating-blackbox.md) | Day-2 runbooks |
| [Internals](docs/internals.md) | Architecture and design map |
| [Knowledge Store](docs/knowledge-store.md) | Learn, decide, remember, render |
| [Bro Runtime](docs/bro-runtime.md) | Dispatch, teams, brofiles, providers |
| [Workflows](docs/workflows.md) | Workflow engine, webhooks, signals |

## Build

```bash
cargo build --release    # binaries at target/release/{blackboxd,bro,bro-slack,bro-irc}
cargo test               # unit tests
```

Nix builds are also available — see the flake (`nix build`, `nix run`, `nix develop`).

## License

MIT
