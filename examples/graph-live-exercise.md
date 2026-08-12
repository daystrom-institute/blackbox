# Graph Live Exercise

`graph-live-exercise.sh` drives the reflective project graph stack end to end
against a throwaway daemon and prints a PASS or FAIL row per step. It is a
live exercise, not a unit test: every step goes through the real surfaces an
operator or an agent would use, over HTTP, against a daemon that owns nothing
outside its throwaway root.

## Rerun

```bash
cargo build --bin blackboxd --bin blackbox
cargo build -p bro-cli --bin bro
cargo build -p bbox-code-collector
cargo build -p bbox-knowledge-source-client --example workspace-capture

examples/graph-live-exercise.sh
```

The run exits nonzero if any step failed. `BBOX_GRAPH_EXERCISE_KEEP=1` keeps
the throwaway root so the captured JSON stays readable; `BBOX_GRAPH_EXERCISE_ROOT`,
`BBOX_GRAPH_EXERCISE_PORT` (default 7299), and `BBOX_GRAPH_EXERCISE_BIN_DIR`
(default `target/debug`) move the run somewhere else. Production state, the
production daemon on port 7264, and the real HOME are never selected: the
daemon boots with an isolated state root, HOME, XDG directories, transcript
roots, and index path below the throwaway root, and the run refuses to start if
its port is already serving.

## What it proves

| Step | What is actually exercised |
|---|---|
| Catalog genesis | `blackbox project-catalog genesis` writes a fresh version-2 catalog on a never-written state bundle, so the daemon boots in catalog mode instead of bridge mode |
| Daemon boot | throwaway `blackboxd` binds its port under a fully isolated environment |
| Producer onboarding | `bbox-code-collector` probes the checkout and onboards it over the authenticated producer channel; the catalog gains the project, a live attachment, and the checkout identity marker |
| Committed candidate | the collector captures the committed `.bbox/knowledge`, `.bbox/gaps`, and `.bbox/graphs` lanes at a real HEAD and drives the publication candidate to Ready |
| Acceptance | `bbox_project_publisher_advance` in candidate mode runs the daemon-side merge gate and establishes the accepted pointer; the graph views are populated by that call, with no restart |
| Published reads | `bbox_project_graph_list` / `_describe` / `_validate` report the accepted generation, its committed descriptor and schema, and a clean validation |
| Published traversal | `bbox_inspect_entity` on a `project_graph_vertex` ref, `bbox_find_paths` across a `gov:CITES` edge from a claim to its evidence, and `bbox_bundle_evidence` over the vertex and the returned path id |
| Binding mint | `bro workspace-binding mint` mints a workspace binding for the onboarded checkout and installs it `0600` in `.bbox/local` |
| Provisional capture | uncommitted working edits (a new `record/case@3` vertex and its `gov:SUPERSEDES` edge) are captured by the real `WorkspaceCaptureClient` presenting the minted token |
| Own versus published | the bound session sees the overlay generation (one more vertex, provisional source, its checkout id) while published visibility still serves the accepted generation |
| Compound ref | the uncommitted vertex resolves as `provisional_project_graph_vertex:<scope hash>:<checkout id>:<graph>:<vertex>`, that compound ref resolves directly, and published visibility refuses the vertex with `error.not_found` |
| Invalid diagnostics | a malformed row appended to the working graph surfaces as a per-graph Invalid provisional generation carrying its validation errors, and does not fall back to the published lane, which stays valid |

The graph fixture is
`crates/bbox-project-graph/tests/fixtures/governance-record/`. Vertex and edge
counts are asserted relative to the published generation rather than to the row
counts of the committed files, because a generation also carries the
schema-derived meta vertices and edges.

Full JSON for every probe lands in `<root>/evidence/`, alongside the daemon log,
the collector logs, and a per-step log. Steps assert on named fields, not on
exit codes.

## Two things the exercise surfaced

Both are recorded here because the script works around them visibly rather than
silently.

**The minted binding file is not shell-sourceable.**
`bro workspace-binding mint` writes `BRO_WORKSPACE_PUBLISHED_SCOPE` as bare
JSON, while `docs/operations-isolated-dev-daemon.md` tells you to load it with
`set -a; . .bbox/local/workspace-binding.env; set +a`. A shell strips the JSON's
double quotes, and the capture client then refuses the scope with `key must be a
string at line 1 column 2`. The exercise reads the three variables out of the
file by explicit key extraction instead of sourcing it.

**A graph read does not materialize the provisional overlay.**
The captured overlay reaches `project_graph_views` when the bound session
recomputes its own knowledge overlay pair. An own-visibility graph call issued
before any knowledge or gap own read still answers from the published lane, and
after a second capture it answers from the previous provisional generation. The
exercise reads own knowledge first, at each point where a fresh capture has to
become visible, and keeps the cold read as evidence
(`evidence/own-graph-list-cold.json`).

## The capture driver

There is no non-agent client that drives a provisional capture:
`WorkspaceCaptureClient::sync_once()` is reachable only from the harness agent
loop, which needs an LLM turn. `crates/bbox-knowledge-source-client/examples/workspace-capture.rs`
is the same construction with no agent loop around it, reading the same three
variables the runbook documents, so the exercise can capture non-interactively.
