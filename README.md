# blackbox

Blackbox stores the evidence and execution state that agents need across
sessions: indexed transcripts and project source, hybrid search, a typed graph,
shared knowledge, work threads and bro execution across providers.

Blackbox runs model turns through a standalone harness. Callers compose reviews,
gates, retries, schedules and integrations in their own code. The daemon keeps
execution, resume, status, cancellation and waits. Workflow and atom engines,
Slack/Badgey integration, reactions and whiteboard execution are retired.
Historical records remain readable.

## Binaries

| Binary | Purpose |
| --- | --- |
| `blackboxd` | Long-lived HTTP MCP corpus and bro service. |
| `blackbox` | Offline administration and project-catalog tools. |
| `bro` | Fleet terminal client and execution controls. |
| `bro-harness` | Standalone model-turn runtime with native tools. |
| `isolate` | Harness-native deterministic tools and code cells. |
| `bbox-code-source-collector` | Checkout-owner file publication. |
| `bbox-transcript-collector` | Source-host native transcript publication. |

## Build and connect

Build in a warm checkout:

```sh
cargo build --release --workspace
scripts/fmt.sh --check
cargo nextest run --workspace
```

Install the binaries your host runs. A remote corpus deployment keeps source
collectors on the hosts that own files and transcripts. Follow
[getting started](docs/getting-started.md), the
[code collector runbook](docs/code-source-collector.md) and the
[native transcript collector runbook](docs/native-transcript-collector.md).
The operator cluster's build, verification and convergence contract is in
[PROJECT.md](PROJECT.md).

The MCP endpoint is `/mcp`. Configure clients with the actual deployment URL and
appropriate credentials. A supplied path identifies caller scope; it does not
grant the remote daemon access to the caller's filesystem. Use collector-backed
corpus refs for remote reads and native harness tools for file, shell and Git work.

## Using the surface

- Retrieve evidence with `bbox_hybrid_search`, inspect exact entity refs with
  `bbox_inspect_entity`, and package supporting refs with `bbox_bundle_evidence`.
- Read conversations with `bbox_search`, `bbox_context` and `bbox_messages`.
  `bbox_tool_calls` pages through indexed historical tool calls.
- Query durable conventions with `bbox_knowledge`. Track active investigation
  state with `bbox_thread`; durable memory changes require operator authority.
- Start a model turn with `bro_exec`, retain its task/session handles and use
  `bro_resume` for continuity. Wait with `bro_wait` or `bro_when_all`; inspect
  `bro_status` before replacing apparently stalled work.
- Discover providers with `bro_providers`; select a provider to list its models.
  `brofiles` lists compact summaries and expands a selected persona on request.
- Install packets, brofiles, simple agents and teams using inline artifact JSON
  or an HTTP(S) URL. Local caller paths are rejected. List before installing.

Responses default to bounded summaries. Follow returned cursors and request
explicit detail when needed. Context/token occupancy describes a model request;
it is not a remaining session work budget or a reason to stop assigning work.

## Documentation

[Docs index](docs/index.md) maps current runbooks.
[Operating guide](docs/operating-blackbox.md) covers health and maintenance.
[Bro runtime](docs/bro-runtime.md) covers execution primitives.
[Artifact catalog](docs/artifact-catalog.md) describes installation and historical receipts.
[Refactor tooling](docs/refactor.md) explains the harness boundary.
[Retirement contract](design/orchestration/bro-execution-boundary-and-retirement.md)
records the removal scope, verification and preserved ownership.
