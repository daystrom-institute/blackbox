# Review the gap backlog

Use `bbox_gaps` to read bounded pages of open gaps. Preserve each gap ID and its
recorded evidence. Group related entries by the underlying missing capability;
keep independent causes separate even when their wording overlaps.

The caller owns this protocol. For large groups, dispatch focused validators
with `bro_exec`, using [the validator lens](agents/gap-cluster-validator.md),
and collect results with `bro_when_all`. Record task/session handles and use
`bro_resume` for corrections. No daemon workflow or atom installation is required.

Merge the returned verdicts into a concise list: still missing, partially fixed,
fixed with evidence, duplicate, or unclear. Cite current source and relevant
history for each disposition. Propose changes before resolving or superseding
records unless the operator already authorized those exact actions. Commit and
push approved repo-owned gap changes using explicit paths.
