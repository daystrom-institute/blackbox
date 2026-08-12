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
| Binding mint | `bro workspace-binding mint` mints a workspace binding for the onboarded checkout, installs it `0600` in `.bbox/local`, and the file's scope survives being sourced by a shell |
| Provisional capture | uncommitted working edits (a new `record/case@3` vertex and its `gov:SUPERSEDES` edge) are captured by `bro workspace-binding capture`, the operator half of the provisional lane |
| Own versus published | a cold own read (no knowledge or gap read first) already serves the captured overlay, and the bound session sees one more vertex with a provisional source and its checkout id while published visibility still serves the accepted generation |
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

## What the exercise surfaced

Both findings are fixed on this branch; the exercise asserts the fixed behavior
rather than working around it.

**Own-lane graph views went stale.** The captured overlay reached
`project_graph_views` only when the bound session recomputed its own knowledge
overlay pair, so a cold own-visibility graph read answered from the published
lane, and a read after a second capture answered from the previous provisional
generation. A finalized capture now installs that workspace's provisional graph
views at the finalize chokepoint, the mirror of what
`bbox_project_publisher_advance` does for the published lane through
`refresh_published_graph_views`, with the same degrade-warn tolerance. The
exercise reads the own lane cold, before any knowledge or gap own read, and
asserts it already serves the captured generation
(`evidence/own-graph-list-cold.json`).

**The minted binding file was not shell-sourceable.**
`bro workspace-binding mint` wrote `BRO_WORKSPACE_PUBLISHED_SCOPE` as bare JSON
while the runbook told you to load it with `set -a; . <file>; set +a`, so a shell
stripped the JSON's double quotes and every capture client refused the scope.
The value is single-quoted now, a unit test round-trips it through a real shell,
and the exercise sources the file the documented way and asserts the sourced
scope parses back to this checkout's published scope.

## The capture driver

The provisional lane had no non-agent client: `WorkspaceCaptureClient::sync_once()`
was reachable only from the harness agent loop, which needs an LLM turn.
`bro workspace-binding capture` is now the operator half. It performs the same
construction the harness does, reading the three minted variables from
`.bbox/local/workspace-binding.env` by default (`--token`, `--daemon-url`,
`--scope`, and `--binding-env` override), which is what lets this exercise
capture non-interactively and what makes the mint, edit, capture, query loop
usable by hand while authoring a schema.
