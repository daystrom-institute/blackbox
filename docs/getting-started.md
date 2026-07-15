# Getting Started

Blackbox runs as three long-lived same-host services plus one private worker
process for each provider session:

| Service | Default endpoint | Owns |
|---|---|---|
| `blackboxd` | `http://127.0.0.1:7264` | Corpus MCP, transcripts, indexes, knowledge, and durable evidence |
| `fleetd` | `http://127.0.0.1:7265` | Live attempts, workers, worktrees, allocation, roster, and control |
| `blackopsd` | `http://127.0.0.1:7266` | Logical agents, definitions, workflows, schedules, mailboxes, and operational intent |
| `bro-harness` | private fleetd Unix socket | One provider loop, local tools, V8 code mode, context, and session log |

The `bro` CLI and Fleet TUI are thin fleetd clients. Each service can restart
without taking ownership from another service. A fleetd replacement pauses
control while existing workers reconnect and replay acknowledged state.

## 1. Build and install

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release --workspace
install -d ~/.local/bin ~/.local/share/blackbox/memories
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/blackbox-corpusd ~/.local/bin/blackbox-corpusd
install -m 755 target/release/blackopsd ~/.local/bin/blackopsd
install -m 755 target/release/fleetd ~/.local/bin/fleetd
install -m 755 target/release/bro-harness ~/.local/bin/bro-harness
install -m 755 target/release/bro ~/.local/bin/bro
install -m 755 target/release/bro-slack ~/.local/bin/bro-slack
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

`blackbox-corpusd` is installed for typed internal-boundary validation. The
public corpus MCP surface is still served by `blackboxd` while AR-001 completes
the remaining handler/state peel. Do not run both binaries against the same
corpus state.

## 2. Start the services

### Linux systemd

Authority-mode fleetd is not a turnkey Linux install yet. Before enabling it,
an operator must install a root-owned launcher at
`/usr/local/libexec/blackbox-worker-sandbox` that implements and passes the
`blackbox-worker-sandbox-v1` self-test and launch protocol in
[`design/bro-harness/leaf-sandbox-isolation.md`](../design/bro-harness/leaf-sandbox-isolation.md).
The repository does not ship that privileged launcher, and fleetd has no
unsandboxed fallback.

Install the units and start the corpus and operational authorities first:

```bash
install -d ~/.config/systemd/user
cp deploy/{blackbox,blackopsd,fleetd}.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox.service blackopsd.service
```

After installing and verifying the conforming launcher, keep this setting in
the fleetd unit and enable fleetd:

```ini
Environment=FLEETD_WORKER_SANDBOX_LAUNCHER=/usr/local/libexec/blackbox-worker-sandbox
```

```bash
systemctl --user enable --now fleetd.service
journalctl --user -u blackbox -u blackopsd -u fleetd -f
```

Without that launcher, blackboxd and blackopsd can serve corpus and operational
state, but live authority dispatch on Linux remains unavailable.

### macOS launchd

macOS fleetd uses a generated Seatbelt profile and refuses an external worker
launcher. Install all replacement binaries first, then sign every daemon and
worker executable with the same persistent code-signing identity used by prior
installs. Do not switch to ad-hoc signing between releases.

```bash
export BLACKBOX_CODESIGN_IDENTITY="your persistent code-signing identity"
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/blackboxd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/blackopsd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/fleetd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/bro-harness
codesign --verify ~/.local/bin/blackboxd ~/.local/bin/blackopsd ~/.local/bin/fleetd ~/.local/bin/bro-harness
```

Render the launchd templates with an absolute home path:

```bash
install -d "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.blackbox.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.blackopsd.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.fleetd.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
```

If an existing plist contains operator-owned secret or account settings, merge
those entries into the rendered replacement before loading it. For first
install, bootstrap in authority dependency order:

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
```

For a binary-only replacement signed by the same identity, restart only its
owner with `launchctl kickstart -k`. If a plist changed, boot out that label,
replace and lint the plist, then bootstrap it again. The fleetd template uses
`AbandonProcessGroup=true`, so reconnectable workers are not killed with the
fleetd process group.

```bash
# Run only the line for the binary that changed.
launchctl kickstart -k "gui/$(id -u)/com.daystrom.blackbox"
launchctl kickstart -k "gui/$(id -u)/com.daystrom.blackopsd"
launchctl kickstart -k "gui/$(id -u)/com.daystrom.fleetd"
```

When a plist changes, this is the replacement shape for its label:

```bash
launchctl bootout "gui/$(id -u)/com.daystrom.fleetd"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
```

### Shared service bearer

The first service creates
`~/.local/state/blackbox/service.token` as an owner-only file. Every route
except `/healthz` and `/readyz` requires it. Peer daemons and `bro` load the
file directly. Export it only for trusted interactive MCP clients:

```bash
export BLACKBOX_SERVICE_TOKEN="$(tr -d '\n' < ~/.local/state/blackbox/service.token)"
```

## 3. Configure all three MCP entries

A client that needs the full product should configure three bearer-authenticated
servers. Corpus tools are not proxies for fleet or operational tools.

### Claude Code

Add the entries under the top-level `mcpServers` object in the Claude config
that your installation reads:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp?surface=interactive",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    },
    "blackbox-fleet": {
      "type": "http",
      "url": "http://127.0.0.1:7265/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    },
    "blackbox-ops": {
      "type": "http",
      "url": "http://127.0.0.1:7266/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    }
  }
}
```

### Codex CLI

Add these entries to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp?surface=interactive"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"

[mcp_servers.blackbox-fleet]
url = "http://127.0.0.1:7265/mcp"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"

[mcp_servers.blackbox-ops]
url = "http://127.0.0.1:7266/mcp"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"
```

Do not use bare `gemini mcp add` or `copilot mcp add` commands that cannot
attach the bearer. If a client version cannot securely inject an Authorization
header from a secret or environment source, use a local authenticated wrapper
or secret-aware bridge. For one-off operator calls, `bro mcp call` reads the
private token file automatically:

```bash
bro mcp call bbox_stats '{}' --daemon-url http://127.0.0.1:7264
bro mcp call bro_roster '{}' --daemon-url http://127.0.0.1:7265
bro mcp call blackops_definition_list '{}' --daemon-url http://127.0.0.1:7266
```

Never put the token in a URL, command history, committed config, or rendered
provider memory.

## 4. Upgrade from the monolith

The differentiated cutover is a breaking authority migration. Before enabling
fleetd and blackopsd as writers:

1. Stop admitting new legacy work and drain or explicitly abandon every live
   monolith task and workflow attempt.
2. Back up the legacy stores, installed artifact catalog, provider/account
   configuration, and service-manager secrets.
3. Install all binaries and service definitions from the same release.
4. Start blackboxd in its default `corpus` role, then blackopsd, then fleetd.
5. Point clients at the three owner endpoints and verify `/readyz` on each.
6. Keep `BLACKBOX_RUNTIME_ROLE=compatibility` only as a bounded rollback path.
   Do not run its legacy writers beside authority-mode fleetd and blackopsd.

blackopsd imports the shipped definitions embedded in its build plus the
installed artifact catalog. There is currently no automatic import of legacy
live tasks, worker leases, logical-agent/mailbox state, workflow runs, waits,
approvals, schedules, or system-event runtime state. The new fleetd and
blackopsd authority stores start fresh. Preserve the old state for audit and
rollback; do not copy legacy `tasks.json` or workflow state into the new stores.
The missing conversion and cutover tooling is tracked as AR-003 in
[`ARCH_RELAYER_LOG.md`](../ARCH_RELAYER_LOG.md).

This limitation is separate from blackboxd's older default-path migration,
which can move selected corpus and knowledge stores into XDG locations when the
new target does not already exist. It does not migrate authority state.

## 5. Bootstrap and render a project

From a client connected to blackboxd:

```text
bbox_bootstrap(project="/absolute/path/to/your/repo")
bbox_render(scope="both", project="/absolute/path/to/your/repo")
```

`bbox_bootstrap` imports hand-authored instruction files and registers the
project for indexing. `bbox_render` projects the durable knowledge store into
provider markdown. Rendering is one-way; the durable store and committed
project `.bbox/knowledge/` files remain authoritative.

## Credential ownership

- blackboxd receives only corpus and embedding credentials, such as a Voyage
  embedding key.
- fleetd discovers provider account credentials from the standard provider
  homes or an owner-only `FLEETD_PROVIDER_CONFIG`. Provider credentials do not
  belong in blackboxd or blackopsd.
- blackopsd owns integration and publish intent. Credentials required by an
  installed integration adapter belong in the blackopsd service environment or
  its dedicated secret resolver, not in blackboxd or a provider worker.
- the shared `service.token` is for trusted local clients and peer daemons. It
  is never a provider credential and never enters a bro-harness worker.

## Key environment variables

| Owner | Variable | Default or purpose |
|---|---|---|
| shared | `BLACKBOX_SERVICE_TOKEN_FILE` | `~/.local/state/blackbox/service.token` |
| blackboxd | `BBOX_PORT` | Corpus MCP/FDR port `7264` |
| blackboxd | `BLACKBOX_RUNTIME_ROLE` | `corpus`; use `compatibility` only during rollback |
| fleetd | `FLEETD_BIND` | Live control and MCP listener `127.0.0.1:7265` |
| fleetd | `FLEETD_MODE` | Binary default `shadow`; service templates select `authority` |
| fleetd | `FLEETD_PROVIDER_CONFIG` | Optional private provider/account file |
| fleetd | `FLEETD_WORKER_SANDBOX_LAUNCHER` | Required conforming launcher on Linux; rejected on macOS |
| client | `FLEETD_URL` | fleetd endpoint used by `bro fleet` and `bro agent` |
| blackopsd | `BLACKOPSD_BIND` | Operational MCP listener `127.0.0.1:7266` |
| blackopsd | `BLACKOPSD_STATE_DIR` | Durable operational authority state |

See [Operating blackbox](operating-blackbox.md) for health checks and rolling
restart behavior, and [Operations](operations.md) for backup and restore.
