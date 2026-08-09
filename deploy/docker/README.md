# Full-daemon runtime image

`Dockerfile.runtime` is the runtime half of the cage build. Compilation happens
outside Docker in the warm native build lane; the image context contains the
release `blackboxd` and `blackbox` binaries plus `system-defaults/memories`.

The image has one target, `blackboxd`. Locality-first moves the complete daemon
off-host and keeps only fleetd plus collectors on checkout machines. There is
no runtime role mask, blackopsd target, checkout connector, or curated MCP
projection.

The deployment must mount one writable volume at `/var/lib/blackbox` and run as
uid/gid 10001. The image pins the state, Tantivy, vector, XDG state, and XDG
data roots under that volume. System memories ship in the installed layout.

Service, collector, and remote-fleet tokens must be staged from Secret volume
symlinks into real owner-only files. `bro-rpc` rejects symlinks, multiple
hardlinks, foreign owners, and group/other permission bits. An init container
should copy each token to the PVC, chown it to 10001:10001, and chmod it 0600.

The full daemon selects a remote fleet with:

```toml
[daemon]
executor = "fleetd"
fleetd_endpoint = "tcp://fleetd-egress.blackbox.svc.cluster.local:7265"
fleetd_token_file = "/var/lib/blackbox/secrets/fleetd.token"
fleetd_worker_home = "/home/on-agent-host"
fleetd_worker_bro_home = "/state/on-agent-host/bro"
```

The egress Service is expected to be a Tailscale operator `ExternalName`
Service targeting the agent machine. fleetd must bind its tailnet address with
the explicit non-loopback grant. Plain non-loopback TCP is unsupported.
The pod must also set `BLACKBOX_MCP_URL` to the daemon's tailnet ingress MCP
URL. A bind-derived `127.0.0.1` URL points back at the agent host when the
harness consumes it and is therefore invalid for off-host execution.

The image defaults to loopback HTTP. Kubernetes must set `BBOX_BIND=0.0.0.0`
and `BBOX_ALLOW_NONLOOPBACK_BIND=1` together, then use `/readyz` and `/healthz`
on port 7264 for readiness and liveness.
