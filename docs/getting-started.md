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
install -m 755 target/release/blackbox  ~/.local/bin/blackbox
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

For normal interactive use, keep one canonical `blackbox` MCP entry and point it
at the `interactive` surface. Switch to `ops` only for setup, lifecycle, or admin
work; add extra aliases only when you intentionally want a restricted surface
such as `readonly`.

**Claude Code** - add to each `~/.claude*/.claude.json`:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp?surface=interactive"
    }
  }
}
```

**Codex CLI** - add to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp?surface=interactive"
```


```json
{
  "mcp": {
    "blackbox": {
      "type": "remote",
      "url": "http://127.0.0.1:7264/mcp?surface=interactive",
      "enabled": true
    }
  }
}
```

**Gemini CLI** - `gemini mcp add blackbox http://127.0.0.1:7264/mcp?surface=interactive`

**Copilot** - `copilot mcp add blackbox http://127.0.0.1:7264/mcp?surface=interactive`

## 4. Enroll a project from its owning checkout

Check `bbox_project_list()` before adding a project. For a remote corpus daemon,
configure the [Code Source Collector](code-source-collector.md) on the checkout
host with an operator-authorized producer and exact published scope. Initialize
missing project scaffolding on that host:

```sh
bbox-code-collector --config /path/to/code-collector.toml init /absolute/path/to/repo
```

Initialization writes `.bbox` locally. Commit the project identity, then run the
configured collector's `once` or `run` command to onboard and publish the source.
Catalog admission, source publication, and index activation are separate steps;
use `bbox_project_list()` and `bbox_doctor()` to inspect progress.

`bbox_bootstrap` is retired. It does not import instructions or enroll remote
checkouts. See [Projects And Code Indexing](projects-code-indexing.md) for local
compatibility and catalog administration limits. Native session history has its
own [transcript collector](native-transcript-collector.md); code collection does
not collect Claude/Codex session files.

## 5. Render approved knowledge on the target host

For project files, call this from a managed bro-harness session bound to the
owning checkout, where the locality client applies the daemon's render plan:

```text
bbox_render(scope="project", project="<project-selector>")
```

For global provider files, run on the operator host that should receive them:

```sh
bro render global --check
bro render global
```

Direct remote MCP calls cannot write the caller's checkout or home directory.
`bbox_render(scope="global")` targets the daemon host and refuses when that host
has no global render authority. Rendering projects approved knowledge into
managed provider markdown; the knowledge store remains its durable source.

## Environment Variables

Transcript root overrides below configure daemon-local discovery only. They do
not enroll roots on another host; use the native transcript collector there.

| Variable | Purpose | Default |
|---|---|---|
| `BBOX_PORT` | HTTP listener port for MCP, tail, roster | `7264` |
| `TRANSCRIPT_SEARCH_ROOTS` | Override account roots (`name=/path,name2=/path2`) | auto-detected |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | Override Codex data dir | `~/.codex` |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | Override tantivy index location | XDG state dir |
| `BLACKBOX_REINDEX_INTERVAL_SECS` | Background reindex interval | `120` |
| `RUST_LOG` | Tracing filter | `transcript_search=info` |
