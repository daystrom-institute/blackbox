# Publish approved knowledge into provider instructions

`bbox_learn` writes knowledge. `bbox_render` projects published renderable
knowledge into managed instruction files. Rendering is not approval, and it
does not import edits from generated files. `render=false` entries remain
searchable without entering provider instructions.

For project output, call `bbox_render(scope="project", project="<project selector>")`.
A session bound to a checkout uses its client's render-locality exchange to
apply there. A call without that workspace binding uses the checkout-owner
collector when one covers the project. A bound call without the locality
exchange refuses; use its supported client or an unbound connection for owner
delivery. Compatibility deployments may apply through daemon checkout authority.

Inspect the returned status and, for owner delivery, per-output dispositions
rather than assuming success. If it returns `render_pending`, use
the returned `operation` with the same project to retrieve its receipt; this
read does not reapply the plan. A later render can make that receipt historical,
so inspect `current`. Hand-authored files can be refused instead of overwritten.
Owner/locality errors require repairing owner enrollment or capabilities, not
writing the caller's similarly named directory by hand.

For global output on an operator host, run there:

```sh
bro render global --check
bro render global
```

The host running that command is the target. By contrast,
`bbox_render(scope="global")` writes the daemon host and can refuse with
`error.global_render_authority` when that host is not a valid target. A remote
daemon cannot refresh every provider home merely because a render call returned.

Before rendering a queued project rule, ensure its source file has reached the
published ref. Inspect the resulting managed output or owner receipt. Correct
bad instructions at their approved knowledge source, then render again.
