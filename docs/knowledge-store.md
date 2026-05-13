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

Render publishes approved standing memory into provider files:

```text
bbox_render(scope="global")
bbox_render(scope="project", project="/repo/x")
bbox_render(scope="both", project="/repo/x")
```

Global render patches host-level files. Project render writes project-local
provider files. Content outside managed markers is preserved, but the managed
region belongs to blackbox.

Do not use render to keep active-work guidance hot. Use pins for that.

## Bootstrap And Review

First-time repo setup:

```text
bbox_bootstrap(project="/repo/x")
```

Bootstrap imports hand-authored instruction files into the store before render.
Absorbed or imported entries may need review:

```text
bbox_review(action="list")
bbox_review(action="approve", id="abcd1234")
bbox_lint()
```

`bbox_absorb` is now a compatibility no-op for the old rendered-file import path.
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
