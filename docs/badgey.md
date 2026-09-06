# Badgey retirement

Badgey execution and its stateful consultant runtime are retired. The daemon
no longer exposes `badgey_*` or `consultant_*` tools or registers a Badgey agent
or atom adapter. Use `bro_exec` or an ordinary named bro agent for explicit work.

Historical notes, threads, knowledge, proposals, and action journals are
preserved. See [the retention contract](consultant-runtime.md) for the inactive
archive boundary. Installed automation and deployment cleanup are separate
from deleting runtime source; retirement does not authorize deleting data.
