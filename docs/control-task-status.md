# Control task status and exact bodies

`GET /control/status/{task_id}` returns a `CallToolResult` containing JSON.
The default `detail=summary` preserves task identity, state, result availability,
context observations, actionable failures, and a bounded assistant result prefix.
It does not return accounting, full progress-report data, or worker transcript
file coordinates.

An optional typed `snapshot` retains dispatch origin, managed-worktree ownership,
workflow ownership, interruption state, and a bounded structured error. It omits
the duplicate `last_message`; deserialize that optional field as `None`.
The top-level `result` is the sole inline assistant body. Large results carry
`resultTruncated`, `resultBytes`, and `resultCursor`. The prefix contains exact
text without an ellipsis, so callers can append subsequent pages directly.

`tail=N` selects a recent event preview, up to 50 events and 4096 serialized JSON
bytes. `eventPreview` identifies requested, returned, and retained counts. Preview
events omit stream partials and thinking blocks and shorten long strings. A
single event that exceeds the aggregate budget is omitted, with zero returned
and an exact-detail retrieval hint; it cannot bypass the response budget.
`eventCount` counts all observed events, including those evicted from the ring.

Choose one exact body with `detail=result`, `report`, `structured_exit`, `stderr`,
or `events`. These responses contain `body.text`, `body.format`, `body.offset`,
`body.total_bytes`, and optional `body.next_cursor`. Repeat the same task and
detail with `cursor=<body.next_cursor>`, concatenate the text exactly, and parse
JSON after the final page when `format=json`. `limit` requests 4 to 4096 UTF-8
bytes; serialized JSON escaping may cause a smaller page. Each body page is also
bounded to 4096 serialized JSON bytes.

Result and stderr are captured text. Reports preserve the complete stored report
including structured data. Structured exits preserve the workflow's exact exit
value. Events are the exact **retained event ring**, not a complete transcript;
these pages include both `retainedEvents` and lifetime `eventCount`. Body cursors
bind the task, detail, and captured content. If content changes, the cursor is
rejected and the caller must restart without it. An empty captured stderr or
event ring is a valid empty body; a missing assistant result or report is an
error. `tail` is summary-only, while `cursor` and `limit` require body detail.
Unknown query fields and detail values are rejected.

Current Fleet clients obtain task state through `/control/roster` snapshots and
SSE deltas, with transcript transport separate from status. They do not read this
endpoint or its optional `bro_protocol::TaskSnapshot`. The compatibility status
projection is also embedded in attached atom-supervision polling; its separate
assistant, report, and provider-event fields are unchanged. Workflow execution
continues to consume full internal `task_result_json` data. No protocol storage
fields or task events are discarded by this response projection.

Tests cover typed snapshot compatibility, worst-case JSON escaping and mirrored
response size, exact Unicode body reassembly, ring eviction counts, and stale
or mismatched cursors. Live Fleet UI validation is unnecessary for this producer
change because the current client does not consume the changed endpoint.
