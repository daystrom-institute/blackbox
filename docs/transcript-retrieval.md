# Transcript Retrieval

Use transcript tools when the answer lives in prior agent conversations: who
said a rule, what a task did, where a failed attempt happened, or what context a
handoff lost.

Use graph tools when the answer depends on connected entities. Use transcript
tools when you need the conversation itself.

## Retrieval Ladder

Start broad:

```text
bbox_search(query="redis locking", project="/repo/x", role="user")
```

Open context around a hit:

```text
bbox_context(file_path="<jsonl>", byte_offset=12345)
```

Inspect the session:

```text
bbox_session(session_id="<session-id>")
bbox_messages(session_id="<session-id>", from_end=true, limit=40)
```

If you need the origin of a standing claim, use cite:

```text
bbox_cite(claim="never kill processes by port")
```

`bbox_cite` returns oldest-first so the origin comes before later repetitions.

## Search

`bbox_search` is for topics when you do not know the session:

```text
bbox_search(
  query="atom binding workflow",
  project="/repo/x",
  role="assistant",
  exclude_self=true,
  limit=10
)
```

Default smart mode broadens adjacent terms for recall. Use quoted phrases for
exact text and `-term` for exclusions. Use `mode="fulltext"` only when you want
raw Tantivy/Lucene syntax.

Filter early:

| Filter | Use |
|---|---|
| `project` | Keep results scoped to the repo you care about. |
| `role` | Find user directives, assistant summaries, tool results, or thinking blocks. |
| `account` | Separate multiple Claude/Codex accounts. |
| `include_subagents` | Include or exclude subagent transcript blocks. |
| `exclude_self` | Avoid echoing the current turn. |
| `source` | Include or exclude a lane (`slack`, `-slack`, `glm,codex`, ...). |
| `author` | Conversation documents: who spoke, by provider user id. |
| `channel` | Conversation documents: one Slack channel by name or id. |

## Conversations (Slack Lane)

Connector-landed Slack conversations are searchable through the same
`bbox_search`. The channel is a first-class coordinate:

```text
bbox_search(query="import mapping", channel="#ops-incident-4565")
bbox_search(query="ops-incident-4565")   # channel names match plain queries
```

A `channel` name (leading `#` accepted) resolves through the current roster to
the stable channel id, so a renamed channel still matches its whole history;
documents stamped with the queried name keep matching directly. A channel id
works as-is.

Conversation hits render channel, author, and a derived Slack permalink. They
have no readable transcript FILE, but `bbox_context` and `bbox_messages` still
apply: both resolve a slack hit's coordinates against the conversation landing
store directly instead of reading a file, exactly as the search breadcrumb
recommends.

```text
bbox_context(file_path="slack:T4MNK2L6A/C0BP8JG21FU", byte_offset=1712345678000200)
bbox_messages(session_id="C0BP8JG21FU/2026-08-10")
```

`bbox_context`'s `file_path` is the hit's `slack:<workspace_id>/<channel_id>`
locator; its `byte_offset` is not a byte position but the target message's
timestamp with the decimal point removed (the same digit-concatenation the
permalink derivation uses) — the value the breadcrumb already fills in for
you. `bbox_messages` pages the whole channel with `file_path="slack:..."`, or
just one day with `session_id="<channel_id>/<date>"` (the per-channel-per-day
bucket a hit's `session_id` carries). Either way, an unenrolled or unknown
channel refuses with a message naming a working `bbox_search(channel=...)`
lane rather than an ENOENT or "Session not found". Narrowing `channel=`
queries and following the permalink to open a message in Slack remain the
other two working paths. A hit whose only match was metadata (the channel
name, lane, or author) shows the start of the message as its excerpt.

## Sessions And Messages

Use `bbox_sessions_list` when recency matters more than text:

```text
bbox_sessions_list(project="/repo/x", limit=20)
```

Use `bbox_session` for metadata: first prompt, project, duration, tool counts,
and transcript file path.

Use `bbox_messages` when you already know the session and need chronological
flow. Tail mode is useful for takeover:

```text
bbox_messages(session_id="<session-id>", from_end=true, limit=80)
```

## Topics

`bbox_topics` is a cheap "what was this session about?" read. It is term
frequency, not summarization:

```text
bbox_topics(session_id="<session-id>")
```

## Reindex And Health

Background reindex runs periodically. Reach for manual tools only when you are
diagnosing freshness:

```text
bbox_stats()
bbox_reindex(full=false)
bbox_embed_status()
```

`bbox_stats` reports indexed document and segment counts, cached up to 60 seconds.
These totals include indexed collected sources and do not depend on transcript
roots being present on the daemon host. The tool does not assess source coverage,
freshness, disk size, or edge totals. Missing local directories or optional edge
state are not evidence of missing source data or zero edges.

Use `full=true` only after schema changes, corruption, or an explicit reason to
throw away incremental assumptions.

## When To Use Graph Instead

Use [Internals](internals.md) and the agentic opening sequence when the question
needs provenance across entities:

- why a line exists
- which decision superseded another
- what thread/session/note produced a commit
- which docs and symbols relate to an artifact

Transcript search finds text. The graph finds paths.
