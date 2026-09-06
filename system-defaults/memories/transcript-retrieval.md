+++
title = "Transcript retrieval — search, cite, context, session, messages"
tags = ["transcripts", "search", "cite", "context", "session", "messages", "retrieval", "runbook"]
order = 19
template = false
+++
# Transcript retrieval — search, cite, context, session, messages

The transcript tools are individually simple, but agents often need the same multi-step workflow:

1. find the right session or span
2. inspect surrounding context
3. cite or summarize the result

This runbook keeps that workflow cold until needed.

## Standard retrieval ladder

### Find by topic

Start with `bbox_search`.

Use when you know the subject but not the session. Add `project`, `role`, or `account` filters early when you already know the likely slice.

By default `bbox_search` uses `mode="smart"`:

- adjacent terms broaden recall
- quoted phrases stay exact
- `-term` excludes

Switch to `mode="fulltext"` only when you want raw Tantivy/Lucene-style boolean syntax and conjunction semantics.

Examples:

- `bbox_search(query="blackbox-dev adversarial", project="transcript-search")`
- `bbox_search(query="redis AND locking", project="transcript-search", mode="fulltext")`
- `bbox_search(query="blackbox-dev -service", project="transcript-search")`

If your question is about stored knowledge rather than transcripts, `bbox_knowledge` uses the same natural query language by default. Reach for `mode="substring"` there only when you want literal whole-query matching.

### Find provenance for a rule

Use `bbox_cite`.

This is better than raw search when the real question is "where did this rule come from?" because it is optimized for origin-finding rather than general recall.

### Expand around a hit

Use `bbox_context`.

Once search or cite gives you a byte offset, pull the surrounding turns instead of re-querying with looser wording.

### Read the conversation flow

Use `bbox_messages`.

This is the right step when context is still too sparse and you need the retained chronological exchange.

### Inspect session metadata

Use `bbox_session`.

This reports retained message count, source identities, and indexed time range for an exact session ID. It does not establish source completeness or freshness.

### Browse without a concrete query

Use `bbox_sessions_list`.

This is the fallback when you only know rough recency, project, or session naming.

## Conversation (Slack) lane divergence

Ingested Slack conversations are searchable through the same `bbox_search`,
and the drill-down half of the ladder now reaches them too: `bbox_context`
and `bbox_messages` resolve a slack hit's coordinates against the
conversation landing store directly rather than reading a transcript file.
The search breadcrumb on a slack top hit already fills these in for you:

- **Surrounding turns**: `bbox_context(file_path="slack:<workspace>/<channel>",
  byte_offset=<digit-encoded message ts>)`. The locator is the hit's
  `file_path`; `byte_offset` is not a byte position but the target message's
  timestamp with the decimal point removed (same digit-concatenation the
  permalink uses) — copy it straight from the breadcrumb rather than deriving
  it by hand.
- **The day's conversation**: `bbox_messages(session_id="<channel>/<date>")`
  — the per-channel-per-day bucket the hit's `session_id` already carries.
- **The whole channel**: `bbox_messages(file_path="slack:<workspace>/<channel>")`.
- **Scope to a channel** with `channel=` (a name, `#name`, or a channel id) on
  `bbox_search` itself. Names resolve through the current roster to the
  stable channel id, so a renamed channel still matches its whole history.
- **Plain queries match channel names**: `bbox_search(query="ops-incident-4565")`
  finds that channel's messages even when no message body names it.
- **Open a message** by following the hit's rendered `Permalink` line.
- **Filter the lane** with `source="slack"` / `source="-slack"`, and who spoke
  with `author=<provider user id>` (`role` only distinguishes human from app).
- A hit whose match was metadata-only (channel name, lane, author) renders
  the start of the message as its excerpt rather than highlighted fragments.
- An unenrolled or unknown channel refuses by name (pointing at a working
  `bbox_search(channel=...)` call) instead of an ENOENT or "Session not
  found" — treat that refusal text as the answer, not a bug.

### "What's in #some-channel about X?"

1. `bbox_search(query="X", channel="#some-channel")`
2. Broaden: `bbox_search(query="...", channel="<channel id from the hit>")`
3. `bbox_context(...)` / `bbox_messages(...)` with the hit's coordinates for
   surrounding turns or the day's flow
4. Follow the permalink for the full thread in Slack

## Common patterns

### "When did we discuss X?"

1. `bbox_search(query="X", project="...")`
2. `bbox_context(...)` or `bbox_messages(...)`
3. `bbox_session(...)` if you need session metadata for the answer

### "Who established this rule?"

1. `bbox_cite(claim="...")`
2. `bbox_context(...)` if you want nearby turns
3. `bbox_messages(...)` if the origin needs more retained messages

### "What was this session about?"

1. `bbox_session(...)`
2. `bbox_topics(...)`
3. `bbox_messages(...)` only if the summary is still ambiguous

## Keep hot vs cold

Keep hot in tool docs:

- search vs cite distinction
- context expands a hit
- session/messages/topics roles

Keep cold here:

- multi-step retrieval ladders
- common query workflows
- how to escalate from coarse search to retained message pages


## Native source limits and continuation

Pass the search hit's `file_path` unchanged: it is an opaque stored locator,
including when it looks like a path. Context and messages never open it on the
daemon. Native replies describe retained indexed projections, which may already
be parser-truncated. For enrolled native sources, `source_freshness` includes
bounded publication and producer observations: `index_matches_published`,
`published_at`, last contact, and completed-scan failure/deferred counts.
Publication time does not establish producer liveness, and a completed scan with
failed or deferred streams does not establish completeness. Untracked legacy
locators report `not_established`. Reindexing cannot recover history that no
producer has delivered; the source owner must publish/backfill retained files
through the configured native transcript collector.

`bbox_context` selects indexed events around an exact offset (default five on
each side, maximum 25), with short previews. Expand with `bbox_messages` using
exactly one of `session_id` or `file_path`. Follow `next_offset`, including when
`from_end=true`; it accounts for byte-limited pages. Native message content is
capped at 12000 stored bytes even with `max_content_length=0`. Preview truncation
is separate from truncation already present in the stored source projection.
