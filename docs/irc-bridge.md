# IRC Bridge

`bro-irc` is a LAN IRC bridge that relays IRC commands into the blackbox
daemon's `bro exec/resume/status` surface. It's designed for couch
steering - operators on a local IRC server can dispatch bros, check task
status, and participate in councils via IRC messages.

The bridge runs as a sidecar process (just like `bro-slack`), connecting
to both an ngircd instance and the blackbox daemon.

## How it works

```
IRC client ──► ngircd ──► bro-irc sidecar ──► POST /irc/{exec,resume,status,...}
                │              │                         │
                │              │ translates:             │ blackboxd
                │              │ !exec <prompt>  ──►     │ bro exec
                │              │ !resume <prompt> ──►    │ bro resume
                │              │ !st                     │ bro status
                │              │ !dash                   │ bro dashboard
                │              │ !team                   │ bro team
                │              │ !council                │ council integration
```

The daemon exposes a dedicated `/irc/*` HTTP route set (not MCP) that
accepts the same parameters as the MCP tools but returns IRC-friendly
output. The bridge formats responses into IRC messages with colored
prefixes and multi-line splitting.

## Setup

### 1. ngircd server

A sample ngircd config ships at `deploy/irc/config/ngircd.conf`. Install
and configure:

```bash
# Arch
pacman -S ngircd

# Edit /etc/ngircd/ngircd.conf - see deploy/irc/config/ngircd.conf for reference
systemctl enable --now ngircd
```

### 2. bro-irc service

```bash
install -m 755 target/release/bro-irc ~/.local/bin/bro-irc
cp deploy/bro-irc.service ~/.config/systemd/user/
systemctl --user enable --now bro-irc.service
```

The service defaults to connecting to ngircd on `192.168.0.251:6667`,
channel `#bros`, and the daemon at `http://127.0.0.1:7264`.

### 3. Configuration

All config is via CLI flags (or environment for secrets):

| Flag | Default | Purpose |
|---|---|---|
| `--irc-host` | `192.168.0.251` | IRC server host |
| `--irc-port` | `6667` | IRC server port |
| `--nick` | `blackbox` | Bot nickname |
| `--channel` | `#bros` | Channel to join |
| `--daemon-url` | `http://127.0.0.1:7264` | blackboxd base URL |
| `--prefix` | `!` | Command prefix |
| `--allow-nicks` | (none - permissive) | Comma-separated nick allowlist |
| `--announce` | `false` | Print command help on startup |

## Commands

All commands are prefixed with `!` (configurable):

| Command | Action | MCP equivalent |
|---|---|---|
| `!exec <prompt>` | Dispatch a bro with the given prompt | `bro_exec` |
| `!resume <prompt>` | Resume the last conversation in this channel | `bro_resume` |
| `!st [task_id]` | Check task status (or list recent) | `bro_status` / `bro_dashboard` |
| `!dash` | Show recent tasks dashboard | `bro_dashboard` |
| `!team [name]` | Show team roster or create a team | `bro_team` |
| `!cancel <task_id>` | Cancel a running task | `bro_cancel` |
| `!council` | Council integration (open, list, post) | `bro_council_*` |

### Examples

```
<operator> !exec audit the rule-packets docs for gaps
<blackbox> [task-3f7a1b2c] dispatched - Claude/Opus 4.7
<blackbox> [task-3f7a1b2c] completed - found 2 missing AST primitives, logged gaps

<operator> !st task-3f7a1b2c
<blackbox> task-3f7a1b2c | completed | claude | 47s | "audit the rule-packets docs for gaps"

<operator> !dash
<blackbox> Recent tasks:
<blackbox> task-3f7a1b2c | completed | claude | 47s | audit docs
<blackbox> task-9d2e4f1a | running   | codex  | 12m  | refactor index module
```

## Council integration

The bridge auto-opens an IRC council when bros are dispatched. The
council is a structured deliberation board where bro outputs are posted
and operators can annotate, vote, and advance through phases - all via
IRC. See the [Workflow Engine](workflows.md) docs for council mechanics.

## Security

By default, the bridge runs in permissive LAN mode (any nick on the IRC
server can issue commands). Set `--allow-nicks` to restrict:

```bash
bro-irc --allow-nicks alice,bob,charlie
```

The `--daemon-url` should always stay loopback (`127.0.0.1:7264`). The
bridge has no authentication of its own beyond the IRC server's
connection model. For exposed deployments, run ngircd with `Password` in
`[Global]` and place the IRC server behind a VPN or firewall.
