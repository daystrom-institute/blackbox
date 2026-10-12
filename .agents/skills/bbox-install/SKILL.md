---
name: bbox-install
description: Install and verify Blackbox on a single host or cluster, including checkout-host satellites, provider accounts, MCP connections and embedding configuration. Use when asked to install, set up, deploy or configure Blackbox, add a checkout host or collector, connect a provider CLI or account, set the embedding provider or key, bring up a throwaway daemon, or when a fresh install does not index, search or dispatch.
---

# Installing and configuring Blackbox

A deployment is one corpus daemon plus satellites on the hosts that hold checkouts and run agent sessions. Most failed installs come from treating it as one binary: the daemon indexes only what a collector on the owning host publishes to it, and an accepted config or a running process is not proof that a boundary works.

| Component | Runs on | Role |
|---|---|---|
| `blackboxd` | One place: a container with a state volume, or a host service | Index, knowledge, threads, MCP endpoint, dispatch control |
| `fleetd` | The host that runs dispatched agent sessions | Supervises the provider sessions the daemon dispatches |
| `bbox-code-collector` | Each host that owns checkouts | Publishes project files and Git history |
| `bbox-transcript-collector` | Each host that owns provider session files | Publishes Claude Code and Codex session history |
| `bro`, `blackbox` | Operator hosts | Dispatch and ops CLI; offline administration CLI |

Commands, config keys and file layouts live in the repository docs linked below, and those are what you copy from. Where a doc disagrees with the code or with a CLI's current schema, the code and the schema win: follow them and report the doc error.

## 1. Inventory what exists

Before changing anything, establish: the daemon origin and its state volume, who owns the service, the checkout hosts, the collectors already configured, the fleet executor, and every provider CLI, account and MCP registration that must connect. Decide whether this is a persistent deployment or an isolated development instance. Reuse the intended instance: a second daemon started against shared state corrupts it.

Anything that restarts or replaces a service other agents depend on needs a read-only scope check and approval for that named service.

## 2. Choose the topology

- **Containerized daemon** (cluster or one container host): the normal shape. The daemon holds no checkouts. Follow [the runtime image contract](../../../deploy/docker/README.md).
- **Host-service daemon on one machine**: daemon, collectors and sessions share a host. Templates are `deploy/blackbox.service` and `deploy/config.toml`.
- **Throwaway daemon for validation**: isolated port and state, nothing persisted. `scripts/dev-isolated-daemon.sh`, described in [operations-isolated-dev-daemon.md](../../../docs/operations-isolated-dev-daemon.md). Use this, never the deployed daemon, to try a change.
- **Isolated agent world**: daemon plus isolated provider homes through the Nix dev harness in [developing-blackbox.md](../../../docs/developing-blackbox.md).

## 3. Work through `docs/getting-started.md` in order

[getting-started.md](../../../docs/getting-started.md) is the deployment map: build, deploy the daemon, install satellites, connect provider CLIs, enroll projects, render knowledge. [operations.md](../../../docs/operations.md) covers configuration and authority. Decide where the build runs first (the `bbox-dev-lanes` skill). The points below are the ones that go wrong.

**Daemon**

- The runtime image is assembled from prebuilt binaries; it does not compile. Supply native binaries for the image's architecture.
- A containerized daemon needs one writable volume and must declare `daemon.no_checkout_authority = true`. Without it the daemon tries checkout-backed fallbacks against paths that do not exist in the container.
- Tokens must be real owner-only files. Symlinks from secret volumes, extra hardlinks and group or other permission bits are rejected.
- The daemon binds loopback by default. `BBOX_BIND` exposes it and is not an authorization gate; exposure, ingress and the bearer requirement are the operator's network-posture decision ([operations.md](../../../docs/operations.md), Port).
- For dispatch to sessions on another host, the daemon selects one remote `fleetd` and needs an MCP URL reachable from that host. A loopback URL points the worker back at itself, and a selected but unreachable `fleetd` does not fall back to local execution. See the fleet supervisor section of [operating-blackbox.md](../../../docs/operating-blackbox.md).
- For a throwaway daemon, changing `BLACKBOX_STATE_DIR` alone is not isolation. Use the script, and pass `--daemon-url` explicitly when probing it.

**Embeddings**

- The daemon reads the Voyage key from its environment. By default that is `VOYAGE_API_KEY`, with `DAYSTROM_VOYAGE_API_KEY` as the fallback; if `embed.toml` overrides `api_key_env`, supply the variable it names instead. Supply it through the deployment's secret mechanism without logging or committing it. The daemon reads it at start.
- Without a working embedding route, search still works but is keyword-only. Confirm with `bbox_embed_status` instead of assuming.
- Routes can use different providers through `embed.toml`. Changing a route's provider or model requires re-embedding that route.

**Satellites**

- Install `blackbox`, `bro`, `fleetd` and both collectors on each checkout host from the same commit as the daemon. macOS satellites are built on macOS; a Linux image does not provide them.
- Configure each collector from its own doc: [code-source-collector.md](../../../docs/code-source-collector.md) and [native-transcript-collector.md](../../../docs/native-transcript-collector.md). Each needs an operator-minted token and a grant on the daemon side.
- Code collection and transcript collection are separate. Code enrollment never collects session history.
- On macOS, follow the host's stable-signing procedure before activating a satellite. Stage and sign a replacement beside the destination; do not overwrite a running binary and sign it in place. If the signing credential is unavailable, ask the operator instead of substituting ad hoc signing.

**Clients and dispatch**

Read [clients.md](clients.md) before connecting provider CLIs or configuring dispatch accounts. Interactive MCP access, dispatch and transcript collection are three independent things: a CLI that can call Blackbox tools is not thereby dispatchable, and its sessions are not thereby collected.

**Projects and knowledge**

- Enroll each requested project with the `bbox-onboard-project` skill, or step 4 of the getting-started doc when that skill is not installed.
- Render existing approved guidance on each host that should consume it: `bro render global --check`, then `bro render global`. Do not create new durable rules as part of an installation.

## 4. Verify each boundary

Check each one independently, since a failure in one does not predict the others.

- **Daemon**: `/readyz`, then `bro mcp call bbox_doctor '{"format":"summary"}' --surface ops`.
- **Clients**: from each configured provider account, not only the installing shell, discover the Blackbox tools and perform a read.
- **Projects**: check publication status (the catalog tools where a catalog is active, `bbox_project_list` otherwise) and search for known project content.
- **Transcripts**: read a known collected session and its source freshness, including collector contact and failed or deferred streams.
- **Embeddings**: `bro mcp call bbox_embed_status '{}' --surface ops`, then a representative hybrid search.
- **Dispatch**, when requested: one small authorized task through the selected account, observed to completion (the `bbox-bro-dispatch` skill).

A first index of a large repository takes a while; report the stage reached instead of calling it a failure.

## What needs the operator

Minting tokens, supplying API keys, choosing network exposure, granting producer scopes, and restarting any shared service. Prepare the exact change, say what it does, and ask.

## Report

The components installed and their versions, the accounts configured, the checks actually completed, and any missing credential or unfinished step.
