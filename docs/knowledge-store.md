# Knowledge Store

Blackbox memory has lanes. Use the lane that matches how long the fact should
matter and whether future agents should see it automatically.

The rendered markdown files are not the source of truth. They are projections
from the store into `CLAUDE.md`, `AGENTS.md`, and `GEMINI.md`.

## Pick The Right Lane

| Need | Tool |
|---|---|
| Standing rule or convention future sessions must load | `bbox_learn` |
| Durable commitment with its reason | `bbox_learn` (`category="convention"`) |
| Searchable fact that should not live in every prompt | `bbox_learn` (`category="memory"`, `render=false`) |
| Retire an entry | `bbox_forget` |
| Active-arc guidance that should disappear when the work ends | the dispatch brief or `bbox_thread` |
| Side-channel executor signal | `bbox_note` |
| Multi-session investigation state | `bbox_thread` |

`bbox_learn` is the only knowledge write lane. `bbox_knowledge` reads entries,
`bbox_forget` retires them, and `bbox_render` publishes them into managed
provider files. Propose durable entries to the operator and get approval before
the write; every stored entry is active and there is no review queue.

The test for a rendered `bbox_learn` entry is simple: would this still be
correct a year from now after the current migration or work item is over? If
no, it belongs in the dispatch brief, a note, or a thread entry.

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

A durable commitment is a `convention` entry whose content states the rule and
the reason for it:

```text
bbox_learn(
  content="Use RocksDB for the cache layer: SQLite locking conflicted with concurrent writer tasks.",
  category="convention",
  scope="project",
  project="/repo/x"
)
```

To change an entry in place, pass its `id` to `bbox_learn`.

## Recall-Only Entries

Use `bbox_learn` with `render=false` for cold facts:

```text
bbox_learn(
  title="port clash",
  content="Port 7263 conflicts with the old helper daemon on host bravo.",
  category="memory",
  render=false
)
```

Recall-only entries are indexed and retrievable, but never rendered into
provider instruction files.

## Forget And Replace

`bbox_forget` retires an entry by deleting it. To replace an entry, query first,
forget the old entry, then learn the new one:

```text
bbox_knowledge(query="cache layer", project="/repo/x")
bbox_forget(id="8a3f12cd")
bbox_learn(content="...", category="convention", scope="project", project="/repo/x")
```

Project entries live in `.bbox/knowledge/<id>.json`, so the git history of that
file carries prior versions and the reason for each change.

## Entry Model

An entry has `id`, `title`, `content`, `category`, `cluster`, `scope`
(`global` or `project`), `project`, `project_id`, `providers` (a per-provider
render filter), `priority` (`critical`, `standard`, or `supplementary`),
`render`, `render_placement` (satellite topic placement), `created_at`, and
`updated_at`. Categories are `profile`, `convention`, `steering`, `build`,
`tool`, `memory`, and `workflow`. Within a rendered category section, entries
are ordered by priority, then title.

Recall telemetry (`recall_count`, `last_recalled`) is host-local and boosts
ranking in hybrid search. For project entries it lives in the gitignored
`.bbox/local/knowledge-stats.json` sidecar, not in the committed entry file.

Older entry files and stores still load. Fields outside the model above are
ignored, a stored `rationale` is appended to `content`, a `decision` category
reads as `convention`, and entries whose stored status is not active or whose
stored expiry has passed are skipped.

## Query

Use `bbox_knowledge` early when prior commitments or rules could change the answer:

```text
bbox_knowledge(query="retry policy", project="/repo/x")
bbox_knowledge(query="sm-persistence-taxonomy")
```

It also surfaces system memories (`sm-*`). For notes, use `bbox_notes`; for
active threads, use `bbox_thread_list`.

## Render

Project render publishes standing memory through the checkout owner.
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
active-work guidance hot; that belongs in the dispatch brief or the work-item
thread.

## Discover Instructions

Rendered files are one-way projections; nothing imports them back into the
store. To inspect existing instructions, discover indexed project-file
references:

```text
bbox_hybrid_search(
  query="AGENTS.md CLAUDE.md GEMINI.md PROJECT.md instructions",
  project="<project-selector>", doc_type="project_file", limit=5
)
```

Expand returned references with `bbox_inspect_entity`. Missing indexed references
do not prove a file is absent: use the checkout owner's file tools when source
coverage is missing. Propose the resulting knowledge entries for operator
approval before saving them through `bbox_learn`. There is no automatic
instruction-file import lane.

## Notes

Executor observations belong in notes:

```text
bbox_note(kind="done", body="Implemented X; tests Y pass.")
bbox_note(kind="blocked", body="Cannot proceed until schema Z is clarified.")
```

Read them through:

```text
bbox_notes(thread_id="thread-abc", full=true)
bbox_notes(project="/repo/x")
```

At a round boundary, sweep project notes, `bbox_gaps` and `bro_dashboard` for
unresolved notes, open gaps, and failed bro tasks.

## Common Mistakes

- Storing phase-specific guidance with `bbox_learn`.
- Writing rendered markdown by hand and expecting it to be the durable source.
- Learning a replacement without forgetting the entry it replaces.
- Using `bbox_note(kind="learned")` for a user-stated rule. User rules belong in
  `bbox_learn`.
- Rendering just to influence one active dispatch. Put it in the dispatch brief
  or the work-item thread.

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
Provider filters and render flags apply to satellites exactly as they do to
inline entries.

Global provider files contain the inline rules and plain-path breadcrumbs. They
do not import `BLACKBOX.md`; it is an optional complete reference.
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
transport uses version 2. Upgrade the daemon, `bro` global-render client, and
`bro-harness` checkout renderer together: older versions cannot safely interpret the satellite contract.
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
