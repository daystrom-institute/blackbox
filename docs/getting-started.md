# Getting Started

This gets a deployment into the normal blackbox shape:

- one corpus daemon (`blackboxd`) running as a containerized workload with one
  state volume
- checkout hosts running only satellites: `fleetd` (which execs `bro-harness`
  per session), the code and transcript collectors, and the `bro` CLI
- every agent CLI pointed at the same MCP endpoint
- one knowledge store rendered back into provider markdown
- project source published into the agentic corpus by its checkout host

Do this once per deployment and once per checkout host, then use the same
daemon from Claude, Codex, Gemini, Copilot, and Vibe.

## 1. Build the binaries

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release    # blackboxd, blackbox
cargo build --release -p bro-cli -p bro-harness -p fleetd \
  -p bbox-code-collector -p bbox-transcript-collector
```

## 2. Deploy the daemon and install the checkout-host satellites

The daemon ships as a runtime image containing `blackboxd`, the offline
`blackbox` CLI and the system memories. Build it and deploy it with one
writable volume as described in
[the runtime image README](../deploy/docker/README.md).

One daemon serves every Claude / Codex / Gemini / Copilot / Vibe CLI. That is
what makes transcript search, knowledge, threads, and bro tasks shared
instead of provider-local.

On each checkout host, install the satellites:

```bash
install -d ~/.local/bin
install -m 755 target/release/blackbox ~/.local/bin/blackbox
install -m 755 target/release/{bro,bro-harness,fleetd} ~/.local/bin/
install -m 755 target/release/{bbox-code-collector,bbox-transcript-collector} ~/.local/bin/
```

Run `fleetd` as a service (`deploy/fleetd.plist` for launchd,
`deploy/fleetd.service` for systemd; see
[the fleet supervisor](operating-blackbox.md#the-fleet-supervisor-fleetd)),
then configure the [code source collector](code-source-collector.md) and the
[native transcript collector](native-transcript-collector.md).

For a throwaway local daemon used in development, see
[Running an Isolated Throwaway blackboxd](operations-isolated-dev-daemon.md).

## 3. Connect each provider CLI to the daemon

For normal interactive use, keep one canonical `blackbox` MCP entry and point it
at the `interactive` surface. Switch to `ops` only for setup, lifecycle, or admin
work; add extra aliases only when you intentionally want a restricted surface
such as `readonly`.

Replace `<daemon-origin>` below with the deployment's MCP origin and supply the
credentials that deployment requires.

**Claude Code** - add to each `~/.claude*/.claude.json`:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "https://<daemon-origin>/mcp?surface=interactive"
    }
  }
}
```

**Codex CLI** - add to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
url = "https://<daemon-origin>/mcp?surface=interactive"
```


```json
{
  "mcp": {
    "blackbox": {
      "type": "remote",
      "url": "https://<daemon-origin>/mcp?surface=interactive",
      "enabled": true
    }
  }
}
```

**Gemini CLI** - `gemini mcp add blackbox https://<daemon-origin>/mcp?surface=interactive`

**Copilot** - `copilot mcp add blackbox https://<daemon-origin>/mcp?surface=interactive`

## 4. Enroll a project from its owning checkout

MCP clients can read the instance-specific onboarding instructions from
`blackbox://skills/onboard-project/SKILL.md`, use the `onboard-project` prompt,
or discover the same skill through `skills/list`.

Check `bbox_project_list()` before adding a project. Configure one producer
with `claim_scopes = "unclaimed"` and configure the
[Code Source Collector](code-source-collector.md) on the checkout host with an
`enroll_roots` entry that contains the project. These are host-level settings,
not per-project entries. Then run, as an operator:

```bash
bro mcp call bbox_project_register '{"path":"/absolute/path/to/repo"}' --surface ops
```

If the daemon cannot stat the path, it routes enrollment to the fresh
checkout-host collector whose most specific enroll root contains it. The
collector scaffolds `.bbox`, records the project in its enrolled-projects
sidecar, and onboards it without a per-project config edit. When the response
reports `identity_committed = false`, commit exactly the returned `commit_paths`
on `published_ref`. Catalog admission, source publication, and index activation
are separate steps; use `bbox_project_list()` and the `ops`-surface
`bbox_doctor` to inspect progress.

See [Projects And Code Indexing](projects-code-indexing.md) for local
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
