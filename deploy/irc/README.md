# Blackbox IRC Bridge

LAN-only couch steering setup.

Start ngIRCd:

```bash
docker compose -f deploy/irc/docker-compose.yml up -d
```

The IRC server is exposed only on `192.168.0.251:6667`. The LinuxServer
ngIRCd image creates `/config/ngircd.conf` on first run; edit
`deploy/irc/config/ngircd.conf` if you want a custom network name, MOTD,
or operator password.

Start the bridge:

```bash
cargo run --bin bro-irc -- \
  --irc-host 192.168.0.251 \
  --irc-port 6667 \
  --channel '#bros' \
  --daemon-url http://127.0.0.1:7264
```

IRC commands:

```text
!thread <team> <topic>
!rooms
!where
!close [council-id]
!attach <council-id>
!dashboard
!status <task-id>
!exec <bro> <prompt>
!provider <provider> <prompt>
!team <team> <prompt>
!resume <bro|provider:session-id> <prompt>
!cancel <task-id>
```

`!thread` creates a fresh council-backed IRC channel named
`#council-<id>` and invites the requesting nick. In that channel,
plain chat messages are posted to the council; no `!team ...` prefix is
needed. The bridge rejoins open council channels on restart.

The bridge wraps IRC-directed prompts with a strict plain-text contract:
no Markdown, no lists, no tables, no headings, and no visible no-op
sentinel.
