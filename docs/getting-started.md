# Getting Started

This gets one host into the normal blackbox shape:

- one long-running `blackboxd`
- every agent CLI pointed at the same MCP endpoint
- one knowledge store rendered back into provider markdown
- project source indexed into the agentic corpus

Do this once per machine, then use the same daemon from Claude, Codex, Gemini,
Copilot, and Vibe.

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
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

## 2. Run `blackboxd` as a systemd user service

```bash
cp deploy/blackbox.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox.service
```

One daemon serves every Claude / Codex / Gemini / Copilot / Vibe CLI on the
host. That is what makes transcript search, knowledge, threads, notes, and bro
tasks shared instead of provider-local.

Prod and dev should use separate installed daemon paths even when they come from
the same built artifact. Dev restarts should never mutate the prod service
binary in place.

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

For normal human/operator use, keep one canonical `blackbox` MCP entry and point
it at the `ops` surface. Add extra aliases only when you intentionally want a
restricted surface such as `readonly`.

**Claude Code** - add to each `~/.claude*/.claude.json`:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp?surface=ops"
    }
  }
}
```

**Codex CLI** - add to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp?surface=ops"
```

**OpenCode** - add to `~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "blackbox": {
      "type": "remote",
      "url": "http://127.0.0.1:7264/mcp?surface=ops",
      "enabled": true
    }
  }
}
```

**Gemini CLI** - `gemini mcp add blackbox http://127.0.0.1:7264/mcp?surface=ops`

**Copilot** - `copilot mcp add blackbox http://127.0.0.1:7264/mcp?surface=ops`

## 4. Bootstrap a project

```bash
bbox_bootstrap(project="/absolute/path/to/your/repo")
```

This onboards the repo into the knowledge system, registers it with the agentic
corpus, and emits structural edges such as file to function and file to class.

## 5. Render the knowledge store

```bash
bbox_render(scope="both", project="/path/to/repo")
```

This writes a unified layered markdown file for each provider:
`~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md`,
and per-project `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`.

Rendering is a projection. The durable source of truth is the blackbox knowledge
store, not the rendered markdown.

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
