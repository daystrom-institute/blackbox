# Graph Live Exercise

`graph-live-exercise.sh` drives the reflective project graph stack end to end
against a throwaway daemon and prints a PASS or FAIL row per step. It is a
live exercise, not a unit test: every step goes through the real surfaces an
operator or an agent would use, over HTTP, against a daemon that owns nothing
outside its throwaway root. It connects on the `ops` MCP surface, which serves
the operator tools (`bbox_project_publisher_*`, `bbox_project_graph_*`,
`bbox_project_catalog_*`) the acceptance and published-read steps call.

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
| Acceptance | the daemon accepts the Ready candidate as it finalizes, running the daemon-side merge gate and establishing the accepted pointer; `bbox_project_publisher_status` names the candidate, and the graph views are populated by that acceptance, with no restart |
| Published reads | `bbox_project_graph_list` / `_describe` / `_validate` report the accepted generation, its committed descriptor and schema, and a clean validation |
| Published traversal | `bbox_inspect_entity` on a `project_graph_vertex` claim ref and on its cited evidence vertex, following the `gov:CITES` edge in both directions |
| Uncommitted edits stay unpublished | uncommitted working edits (a new `record/case@3` vertex, its `gov:SUPERSEDES` edge, and a malformed row) leave the graph tree dirty, yet `bbox_project_graph_validate` still reports the published graph valid with no errors, `bbox_inspect_entity` refuses the uncommitted vertex with `error.not_found`, and `bbox_project_graph_list` refuses the removed `provisional` parameter as an unknown field |

The graph fixture is
`crates/bbox-project-graph/tests/fixtures/governance-record/`. Vertex and edge
counts are asserted relative to the published generation rather than to the row
counts of the committed files, because a generation also carries the
schema-derived meta vertices and edges.

Full JSON for every probe lands in `<root>/evidence/`, alongside the daemon log,
the collector logs, and a per-step log. Steps assert on named fields, not on
exit codes.

## Reads are published-only

Every graph read serves the accepted generation. A working edit, valid or
malformed, is visible to no read until it is committed and accepted; there is
no local validator, so an uncommitted graph is validated after commit, by the
merge gate over the candidate tree and by the accepted view build that
`bbox_project_graph_validate` reports.
