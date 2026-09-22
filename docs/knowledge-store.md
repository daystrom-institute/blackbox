# Knowledge Store

Blackbox memory has lanes. Use the lane that matches how long the fact should
matter and whether future agents should see it automatically.

The rendered markdown files are not the source of truth. They are projections
from the store into `CLAUDE.md`, `AGENTS.md`, and `GEMINI.md`.

## Pick The Right Lane

| Need | Tool |
|---|---|
| Standing rule or convention future sessions must load | `bbox_learn` |
| Durable decision with rationale and possible supersession | `bbox_decide` |
| Searchable fact that should not live in every prompt | `bbox_remember` |
| Active-arc guidance that should disappear when the work ends | `bbox_pin` |
| Side-channel executor signal | `bbox_note` |
| Multi-session investigation state | `bbox_thread` |

The test for `bbox_learn` is simple: would this still be correct a year from
now after the current migration or work item is over? If no, it is probably a
pin, note, or thread entry.

## Learn

Use `bbox_learn` for user-stated rules:

```text
bbox_learn(
  content="Use rustls, not openssl, for this crate.",
  category="convention",
  scope="project",
  project="/repo/x"
)
```

Learned entries render into provider markdown after `bbox_render`. They are for
future agent behavior, not for observations you discovered yourself.

## Decide

Use `bbox_decide` when the rationale matters:

```text
bbox_decide(
  content="Use RocksDB for the cache layer.",
  rationale="SQLite locking conflicted with concurrent writer tasks.",
  scope="project",
  project="/repo/x"
)
```

If you are replacing a prior decision, query first and supersede the old entry:

```text
bbox_knowledge(query="cache layer", project="/repo/x")
bbox_decide(content="...", rationale="...", supersedes="8a3f12cd")
```

Supersession is the audit trail. Do not delete a decision just because it changed.

## Remember

Use `bbox_remember` for cold facts:

```text
bbox_remember(
  title="port clash",
  content="Port 7263 conflicts with the old helper daemon on host bravo."
)
```

Remembered facts are indexed and retrievable, but not rendered into provider
instruction files.

## Pin

Pins are hot context for one execution lane:

```text
bbox_pin(
  action="set",
  scope="thread",
  target="thread-abc123",
  project="/repo/x",
  title="Migration phase",
  content="For this phase, do not migrate callers outside module A."
)
```

Pins survive daemon restarts, but they are injected only when the dispatch
matches their scope. They are the right place for phase notes, temporary
executor charters, and "for this arc only" constraints.

List pins explicitly:

```text
bbox_pin(action="list", project="/repo/x")
```

They do not show up through `bbox_knowledge`.

## Query

Use `bbox_knowledge` early when prior decisions or rules could change the answer:

```text
bbox_knowledge(query="retry policy", project="/repo/x")
bbox_knowledge(query="sm-persistence-taxonomy")
```

It also surfaces system memories (`sm-*`) and matching packets. For scoped pins,
use `bbox_pin(action="list")`; for notes, use `bbox_notes`; for active threads,
use `bbox_thread_list`.

## Render

Project render publishes approved standing memory through the checkout owner.
In a managed bro-harness session bound to that checkout:

```text
bbox_render(scope="project", project="<project-selector>")
```

The locality client applies the daemon's plan to project provider files. Direct
remote MCP cannot apply files in the caller's checkout. For global provider
files, run `bro render global` on the operator host (`--check` previews changes).
`bbox_render(scope="global")` instead targets the daemon host and refuses when
that host lacks global render authority.

Content outside managed markers is preserved. Do not use render to keep
active-work guidance hot; use pins for that.

## Discover Instructions And Review Entries

`bbox_bootstrap` is retired and never imported knowledge. To inspect existing
instructions, discover indexed project-file references:

```text
bbox_hybrid_search(
  query="AGENTS.md CLAUDE.md GEMINI.md PROJECT.md instructions",
  project="<project-selector>", doc_type="project_file", limit=5
)
```

Expand returned references with `bbox_inspect_entity`. Missing indexed references
do not prove a file is absent: use the checkout owner's file tools when source
coverage is missing. Propose the resulting knowledge entries for operator
approval before saving them through `bbox_learn`, `bbox_decide`, or
`bbox_remember`. There is no automatic instruction-file import lane.

Review accepts or rejects entries already awaiting approval in the store:

```text
bbox_review(action="list")
bbox_review(action="approve", id="abcd1234")
bbox_lint()
```

`bbox_absorb` is a compatibility no-op for the old rendered-file import path.
Rendered files are one-way projections.

## Notes And Inbox

Executor observations belong in notes:

```text
bbox_note(kind="done", body="Implemented X; tests Y pass.")
bbox_note(kind="blocked", body="Cannot proceed until schema Z is clarified.")
```

Read them through:

```text
bbox_notes(thread_id="thread-abc", full=true)
bbox_inbox(project="/repo/x")
```

The inbox aggregates unresolved notes, stale threads, unverified knowledge,
deferred followups, and failed bro tasks. It is the round-boundary sweep.

## Common Mistakes

- Storing phase-specific guidance with `bbox_learn`.
- Writing rendered markdown by hand and expecting it to be the durable source.
- Forgetting to query before superseding a decision.
- Using `bbox_note(kind="learned")` for a user-stated rule. User rules belong in
  `bbox_learn` or `bbox_decide`.
- Rendering just to influence one active dispatch. Use `bbox_pin`.

## Instruction placement and satellites

`render: false` remains indexed-only. Rendered entries default to inline placement;
existing entries are never silently summarized or moved. To explicitly defer an
entry, use `bbox_learn` with `render_placement` (or edit the repo-owned source):

```json
{"render_placement": {"placement": "satellite", "topic": "build"}}
```

`{"placement":"inline"}` restores inline placement. An omitted placement on an
update preserves the current value. Topics are a closed, code-owned registry:
`retrieval`, `persistence`, `orchestration`, `operations`, `build`, `architecture`,
`refactoring`, `authoring`, and `shell`. Each has a conditional loading cue;
entries cannot add their own unconditional loading instructions to the index.
Provider filters, provider variants, approval markers, expiry, and render flags
apply to satellites exactly as they do to inline entries.

Global provider files contain the inline rules and plain-path breadcrumbs. They
no longer import `BLACKBOX.md`; it remains an optional complete reference.
Generated Blackbox tool procedures are split by topic. Local global rendering
and host-applied `bro render global` share the same complete plan. A provider
entrypoint over 6,000 bytes reports a diagnostic rather than truncating rules.

Satellites live below `guidance/<content-hash>/` beside the global common file,
or below `.bbox/guidance/<content-hash>/` in a project. Their source remains the
knowledge entries and code-owned tool catalog, not the generated markdown.
The applier validates all satellite paths and contents, writes satellites first,
and only then publishes entrypoints. Previous generations remain available for
rollback and in-flight readers. A conflicting immutable file or symlink refuses
the render. Global managed regions preserve surrounding hand-authored text and
retain the destructive-shrink guard. An explicit relocation can pass that guard
only when every old nonempty line remains in that provider's entrypoint or a
referenced satellite; mechanically updated content-addressed breadcrumbs are
matched by their unchanged cue and filename. Project entrypoints retain their
existing generated-file ownership check.

Global render plans use wire kind `bbox.global_render_plan.v2`; project render
transport uses version 2. Upgrade the daemon and applying client/collector
together: older versions cannot safely interpret the satellite contract.
`bro render global` assembles checksum-bound pages before writing any files;
a stale generation restarts delivery from page one, with bounded retries.
Project receipts and drift checks cover satellites as well as entrypoints.

`PROJECT.md` is conditional project orientation, with repository-relative links
to authored guides. It is not included automatically. To inspect or regenerate
project guidance offline, without reading the live knowledge store:

```sh
cargo run -p bbox-tool-docs --example render_guidance -- /path/to/repo /path/to/output
```

Use a separate output directory for a preview. Using the source directory as the
output regenerates that checkout's project projections from `.bbox/knowledge`.
Generated project satellites are committed alongside their source entries.
