# Transcript retrieval ladder

1. Find a topic with `bbox_hybrid_search(query=..., doc_type="transcript",
   project=..., limit=5)`. When known, narrow by `source`, `account`, `role`,
   `author`, or `channel`. Conversation filters scope the conversation lane;
   a bare project filter on a mixed search does not filter all entity types.
   For a user's stated rule, an exact quoted phrase with `role="user"` is useful.
2. Copy the hit's `exact_read` arguments when present. Otherwise pass
   `conversation.file_path` and `conversation.byte_offset` unchanged to
   `bbox_context`. These are opaque stored coordinates, not files to open or
   offsets to calculate. Ordinary native context gives short previews of nearby
   indexed events, with `context_lines` defaulting to five on each side and
   capped at 25.
3. For an exact native record, execute the supplied `bbox_context` recovery
   handle and offset, then concatenate `body.text` and follow `body.next_cursor`
   as `body_cursor` with the same selectors. Omit `context_lines` on exact body
   reads. If the handle becomes invalid after source replacement, rediscover it.
4. For chronological messages, call `bbox_messages` with exactly one of
   `session_id` or `file_path`. Follow `next_offset`, including with
   `from_end=true`; do not calculate the next page from the requested limit.
   Native `max_content_length=0` still caps each message at 12,000 retained
   bytes. Use row `exact_read` hints to recover more stored content.
5. Use `bbox_session(session_id=...)` for retained count, source identity, and
   indexed time range. Browse `bbox_sessions_list` when only recency, project,
   source, or account is known. Session-name filtering is unavailable.

For retained Slack conversations, copy the returned channel locator and numeric
message coordinate. `bbox_messages(file_path=...)` selects the channel;
the returned channel/date `session_id` selects a day. Use the returned permalink
for attribution. Native exact-record recovery handles do not apply to Slack.
Reading these records does not authorize posting or reacting in Slack.

Native reads expose indexed projections. Neither a session count nor successful
exact paging proves original-source completeness. Check `source_freshness`:
index/publication agreement, publication time, producer contact, and completed
scan failure/deferred counts measure different things. `not_established` means
freshness was not established. Do not substitute a daemon reindex for missing
producer publication or backfill.
