# bbox-system-events: durable bro observation journal

Invariants from the line-10902 journal-corruption incident (a foreign
event's bytes spliced mid-string into another record, which emptied the
boot index and wedged compaction for weeks while the journal grew past
its retention cap).

- **Journal appends are one buffer, one `write_all`.** Never serialize
  directly into the `File` (`serde_json::to_writer` issues many small
  writes); the in-process mutex does not protect against a second process
  on the same `BRO_HOME`, and O_APPEND atomicity only holds per syscall.
- **Journal reads are lenient; malformed lines are never fatal.** A bad
  line is skipped with a warning on load (`LoadedJournal.malformed`) and
  quarantined raw to `corrupt.jsonl` + scrubbed by the next compaction —
  which forces a rewrite even when retention drops nothing. All-or-nothing
  parsing is the failure mode that turned one bad line into an empty
  index and unbounded journal growth.
- `EventStore::build` swallows load errors (`if let Ok`) — any new load
  failure mode silently empties the in-memory index. Keep failures
  per-line, not per-file.

Recording an event only appends the journal and broadcasts the observation.
There is no reaction matching, outbox, identity provisioning or execution.
Historical reaction/outbox/identity directories remain untouched archives;
opening the observation hub must neither parse them nor create replacements.
Journal retention is mechanical service maintenance, independent of workflows.
