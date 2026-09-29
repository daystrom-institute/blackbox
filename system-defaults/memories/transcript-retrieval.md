+++
title = "Transcript retrieval - search, context, session, messages"
tags = ["transcripts", "search", "context", "session", "messages", "retrieval", "runbook"]
order = 19
template = false
+++
# Transcript retrieval - search, context, session, messages

The transcript tools are individually simple, but agents often need the same multi-step workflow:

1. find the right session or span
2. inspect surrounding context
3. cite or summarize the result

This runbook keeps that workflow cold until needed.

## Standard retrieval ladder

### Find by topic

Start with `bbox_hybrid_search`.

Use when you know the subject but not the session. Add `doc_type="transcript"`, `project`, `role`, or `account` filters early when you already know the likely slice; any conversation filter narrows the search to conversations, and filters apply to every ranking lane before results are ranked.

By default `bbox_hybrid_search` uses `mode="smart"`:

- adjacent terms broaden recall
- quoted phrases stay exact
- `-term` excludes

Switch to `mode="fulltext"` only when you want raw Tantivy/Lucene-style boolean syntax and conjunction semantics.

Examples:

- `bbox_hybrid_search(query="blackbox-dev adversarial", project="transcript-search", doc_type="transcript")`
- `bbox_hybrid_search(query="redis AND locking", project="transcript-search", doc_type="transcript", mode="fulltext")`
- `bbox_hybrid_search(query="blackbox-dev -service", project="transcript-search", doc_type="transcript")`

Each transcript hit carries a `conversation` object (`session_id`, `file_path`, `byte_offset`, `timestamp`, `account`, `source`, `project`, ...) and, when an indexed recovery handle exists, an `exact_read` that is a ready `bbox_context` call. `next_steps` lists the follow-up readers.

If your question is about stored knowledge rather than transcripts, `bbox_knowledge` uses the same natural query language by default. Reach for `mode="substring"` there only when you want literal whole-query matching.

### Find provenance for a rule

Search user turns for the quoted phrase: `bbox_hybrid_search(query="\"<phrase>\"", role="user")`.

The quoted phrase keeps the match exact and `role="user"` keeps the hits on the turns where a rule was stated rather than where it was repeated back.

### Expand around a hit

Use `bbox_context`.

Once search gives you a hit, run its `exact_read` (or `bbox_context` with `conversation.file_path` and `conversation.byte_offset`) to pull the surrounding turns instead of re-querying with looser wording.

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

Ingested Slack conversations are searchable through the same
`bbox_hybrid_search`, and the drill-down half of the ladder reaches them too:
`bbox_context` and `bbox_messages` resolve a slack hit's coordinates against the
conversation landing store directly rather than reading a transcript file.
A slack hit's `conversation` object and `next_steps` already fill these in for
you:

- **Surrounding turns**: `bbox_context(file_path="slack:<workspace>/<channel>",
  byte_offset=<digit-encoded message ts>)`. The locator is the hit's
  `conversation.file_path`; `byte_offset` is not a byte position but the target
  message's timestamp with the decimal point removed (same digit-concatenation
  the permalink uses): copy it straight from `conversation.byte_offset` rather
  than deriving it by hand.
- **The day's conversation**: `bbox_messages(session_id="<channel>/<date>")`,
  the per-channel-per-day bucket `conversation.session_id` already carries.
- **The whole channel**: `bbox_messages(file_path="slack:<workspace>/<channel>")`.
- **Scope to a channel** with `channel=` (a name, `#name`, or a channel id) on
  `bbox_hybrid_search` itself. Names resolve through the current roster to the
  stable channel id, so a renamed channel still matches its whole history.
- **Plain queries match channel names**: `bbox_hybrid_search(query="ops-incident-4565", doc_type="transcript")`
  finds that channel's messages even when no message body names it.
- **Open a message** by following the hit's `conversation.permalink`.
- **Filter the lane** with `source="slack"` / `source="-slack"`, and who spoke
  with `author=<provider user id>` (`role` only distinguishes human from app).
- A hit whose match was metadata-only (channel name, lane, author) carries
  the start of the message as its excerpt rather than highlighted fragments.
- An unenrolled or unknown channel refuses by name (pointing at a working
  `bbox_hybrid_search(channel=...)` call) instead of an ENOENT or "Session not
  found": treat that refusal text as the answer, not a bug.

### "What's in #some-channel about X?"

1. `bbox_hybrid_search(query="X", channel="#some-channel")`
2. Broaden: `bbox_hybrid_search(query="...", channel="<channel id from the hit>")`
3. `bbox_context(...)` / `bbox_messages(...)` with the hit's coordinates for
   surrounding turns or the day's flow
4. Follow the permalink for the full thread in Slack

## Common patterns

### "When did we discuss X?"

1. `bbox_hybrid_search(query="X", project="...", doc_type="transcript")`
2. `bbox_context(...)` or `bbox_messages(...)`
3. `bbox_session(...)` if you need session metadata for the answer

### "Who established this rule?"

1. `bbox_hybrid_search(query="\"...\"", role="user")`
2. `bbox_context(...)` if you want nearby turns
3. `bbox_messages(...)` if the origin needs more retained messages

### "What was this session about?"

1. `bbox_session(...)`
2. `bbox_messages(...)` for the retained turns

## Keep hot vs cold

Keep hot in tool docs:

- context expands a hit
- session/messages roles

Keep cold here:

- multi-step retrieval ladders
- common query workflows
- how to escalate from coarse search to retained message pages


## Native source limits and continuation

Pass the search hit's `conversation.file_path` unchanged: it is an opaque stored
locator, including when it looks like a path. Context and messages never open it
on the daemon. Native replies describe retained indexed projections, which may
already be parser-truncated. For enrolled native sources, `source_freshness`
includes bounded publication and producer observations:
`index_matches_published`, `published_at`, last contact, and completed-scan
failure/deferred counts. Publication time does not establish producer liveness,
and a completed scan with failed or deferred streams does not establish
completeness. Untracked legacy locators report `not_established`. Reindexing
cannot recover history that no producer has delivered; the source owner must
publish/backfill retained files through the configured native transcript
collector.

`bbox_context` selects indexed events around an exact offset (default five on
each side, maximum 25), with short previews. Expand with `bbox_messages` using
exactly one of `session_id` or `file_path`. Follow `next_offset`, including when
`from_end=true`; it accounts for byte-limited pages. Native message content is
capped at 12000 stored bytes even with `max_content_length=0`. Preview truncation
is separate from truncation already present in the stored source projection.
