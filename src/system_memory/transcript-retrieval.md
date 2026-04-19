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

### Find provenance for a rule

Use `bbox_cite`.

This is better than raw search when the real question is "where did this rule come from?" because it is optimized for origin-finding rather than general recall.

### Expand around a hit

Use `bbox_context`.

Once search or cite gives you a byte offset, pull the surrounding turns instead of re-querying with looser wording.

### Read the conversation flow

Use `bbox_messages`.

This is the right step when context is still too sparse and you need the actual chronological exchange.

### Inspect session metadata

Use `bbox_session`.

This is useful before reading the full conversation when you want to confirm you found the right session by project, first prompt, tool usage, or duration.

### Browse without a concrete query

Use `bbox_sessions_list`.

This is the fallback when you only know rough recency, project, or session naming.

## Common patterns

### "When did we discuss X?"

1. `bbox_search(query="X", project="...")`
2. `bbox_context(...)` or `bbox_messages(...)`
3. `bbox_session(...)` if you need session metadata for the answer

### "Who established this rule?"

1. `bbox_cite(claim="...")`
2. `bbox_context(...)` if you want nearby turns
3. `bbox_messages(...)` if the origin needs full replay

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
- how to escalate from coarse search to full replay
