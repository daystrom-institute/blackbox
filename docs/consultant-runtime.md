# Consultant runtime retirement

The daemon no longer serves stateful consultant or Badgey tools, adapter
execution, queued turns, proposal application, or startup recovery. Use
`bro_exec` or an ordinary named bro agent for explicit work.

Existing proposal and action-journal files under the configured bro store's
`badgey/proposals` and `badgey/action_journal` remain inactive archives in place.
Runtime removal does not delete, move, or reconcile those records. Historical
notes, threads, knowledge, task results, and project-catalog proposal inventory
remain available through their retained owners. Consultant atom records remain
decodable but cannot be installed or executed.

The [original design](../design/orchestration/agents/consultant-runtime.md) is
historical context. It is not a current setup or dispatch guide.
