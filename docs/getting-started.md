# Getting Started

Five steps. After step 5 every agent CLI on your host is talking to the
same daemon, your existing `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`
content has been absorbed into one store, and the store is rendered back
out to each provider in a consistent layered form.

## 1. Build and install the binaries

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/bro       ~/.local/bin/bro
install -m 755 target/release/bro-slack ~/.local/bin/bro-slack
install -m 755 target/release/bro-irc   ~/.local/bin/bro-irc
```

## 2. Run `blackboxd` as a systemd user service

```bash
cp deploy/blackbox.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox.service
```

One daemon serves every Claude / Codex / Gemini / Copilot / Vibe CLI on
the host, so they all share the same tantivy index, knowledge store, and
orchestration state. Prod and dev should use separate installed daemon
paths even when they come from the same built artifact — dev restarts
never mutate the prod service binary in place.

Logs live in journald:

```bash
journalctl --user -u blackbox -f
```

### Dev daemon (optional, isolated)

```bash
cp deploy/blackbox-dev.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox-dev.service
```

## 3. Connect each provider CLI to the daemon

**Claude Code** — add to each `~/.claude*/.claude.json`:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp"
    }
  }
}
```

**Codex CLI** — add to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp"
```

**Gemini CLI** — `gemini mcp add blackbox http://127.0.0.1:7264/mcp`

**Copilot** — `copilot mcp add blackbox http://127.0.0.1:7264/mcp`

## 4. Bootstrap a project

```bash
bbox_bootstrap(project="/absolute/path/to/your/repo")
```

This onboards the repo into the knowledge system, registers it with the
agentic corpus, and emits structural edges (file → function, file →
class, etc.).

## 5. Render the knowledge store

```bash
bbox_render(scope="both", project="/path/to/repo")
```

This writes a unified layered markdown file for each provider:
`~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md`,
and per-project `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`.

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `BBOX_PORT` | HTTP listener port for MCP, tail, roster | `7264` |
| `TRANSCRIPT_SEARCH_ROOTS` | Override account roots (`name=/path,name2=/path2`) | auto-detected |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | Override Codex data dir | `~/.codex` |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | Override tantivy index location | XDG state dir |
| `BLACKBOX_REINDEX_INTERVAL_SECS` | Background reindex interval | `120` |
| `CLAUDE_BIN` / `OPENCODE_BIN` / `CODEX_BIN` / `COPILOT_BIN` / `GEMINI_BIN` | Override provider binary paths | auto-resolved |
| `RUST_LOG` | Tracing filter | `transcript_search=info` |
