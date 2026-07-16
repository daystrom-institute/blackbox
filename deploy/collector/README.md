# bbox-collector: satellite host setup

`bbox-collector` is a slim log-shipper that tails interactive `claude` and
`codex` transcript roots on a source machine and ships byte increments to the
corpus host's `POST /internal/records`. It needs no fleetd and no bro plane: a
machine running only interactive CLIs ships transcripts with just this binary
installed. Design: `design/daemon-runtime/remote-corpus-host.md` (slice 2c).

Delivery is at-least-once with the corpus server as the cursor authority; the
provider's own session file is the durable backlog (no local spool), so an
unreachable corpus just means retry next tick.

## 1. Build

From a checkout of the workspace (any machine with the Rust toolchain):

```bash
cargo build --release -p bbox-collector
install -m 0755 target/release/bbox-collector ~/.local/bin/bbox-collector
```

Cross-building for a satellite of a different arch is fine; the binary is
self-contained (rustls, no OpenSSL).

## 2. Provision the service token

The collector authenticates to the corpus host with the same 64-hex shared
service token the corpus services use. Copy it from the corpus host (or your
secret store) into an owner-only file on the satellite:

```bash
install -d -m 0700 ~/.local/state/blackbox
umask 077
cp /secure/transfer/service.token ~/.local/state/blackbox/service.token
chmod 0600 ~/.local/state/blackbox/service.token
```

The collector fails closed at startup if this file is missing or group/world
readable.

## 3. Configure

```bash
install -d -m 0700 ~/.config/blackbox
cp deploy/collector/collector.example.toml ~/.config/blackbox/collector.toml
$EDITOR ~/.config/blackbox/collector.toml
```

Set `corpus_url`, `service_token_file`, and your `claude_roots` / `codex_root`.
Leave `host_id` unset to derive-and-persist a stable id, or pin one per machine
(it is stamped into the wire producer `collector:<host-id>`, so it must be
unique across satellites). Every field has an env override
(`BBOX_COLLECTOR_*`); see the example file.

## 4a. Install as a launchd agent (macOS)

```bash
sed "s|__HOME__|$HOME|g" deploy/collector/com.daystrom.bbox-collector.plist.in \
  > ~/Library/LaunchAgents/com.daystrom.bbox-collector.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.daystrom.bbox-collector.plist
launchctl kickstart -k gui/$(id -u)/com.daystrom.bbox-collector
```

Logs land in `~/Library/Logs/bbox-collector.log`. The agent has
`KeepAlive{SuccessfulExit:false}`: a clean exit is not auto-restarted, a crash
is. To stop it: `launchctl bootout gui/$(id -u)/com.daystrom.bbox-collector`.

## 4b. Install as a systemd user unit (linux)

```bash
install -m 0644 deploy/collector/bbox-collector.service \
  ~/.config/systemd/user/bbox-collector.service
systemctl --user daemon-reload
systemctl --user enable --now bbox-collector.service
journalctl --user -u bbox-collector -f
```

## Operating notes

- **First run is the migration.** A fresh collector starts from cursor zero and
  ships each transcript's full history; the server dedupes by byte range, so it
  is safe to (re)start at any time. Startup resync adopts the server's
  acknowledged tails first, so a re-provisioned satellite does not re-ship.
- **What ships.** Claude `<root>/projects/**.jsonl` and `<root>/history.jsonl`
  per account root, and codex `<root>/sessions/**.jsonl`. Gemini snapshots and
  the fleet harness lane are out of scope in v1.
- **Tuning.** `poll_interval_secs` (default 30) sets the tick cadence and caps
  the reconnect backoff. Raise it on a quiet machine, lower it for tighter
  freshness.
