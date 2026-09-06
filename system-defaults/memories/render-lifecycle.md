+++
title = "Render lifecycle: render, review, lint"
tags = ["render", "absorb", "review", "lint", "knowledge", "lifecycle", "runbook"]
order = 20
template = false
+++
# Render lifecycle: render, review, lint

The render path is easy to misuse because the verbs are adjacent in the UI but operate on different parts of the lifecycle.

This is the compact model:

- `bbox_render` publishes approved knowledge into managed files.
- Rendered files are unidirectional projections. `bbox_absorb` is a retired compatibility no-op and cannot import edits.
- `bbox_review` accepts or rejects entries already awaiting approval in the store.
- `bbox_lint` checks the store for contradictions, duplication, and stale structure.
- `bbox_pin` is not part of this lifecycle. Pins stay out of rendered memory entirely.

## Normal forward path

1. create or update entries with `bbox_learn` / `bbox_decide` / `bbox_remember`
2. `bbox_render`
3. agents consume the managed output

This is the default path when the source of truth is the knowledge store.

## Reverse path after manual edits

Managed regions are regenerated from knowledge. To retain an intentional edit,
update its source entry through the knowledge tools, then render again. Review
controls unverified entries already present in the store; it does not import
rendered files. `bbox_bootstrap` is retired and does not import instructions.
Discover indexed instruction references with `bbox_hybrid_search`, then expand
with `bbox_inspect_entity`, or read missing source through the checkout owner's
file tools. Missing indexed references do not establish absence. Propose entries
for operator approval before saving them; there is no automatic import lane.

## Scope thinking

### Global render

Use when the guidance should land in the provider-level managed files and affect every project/session on the host.

`bbox_render(scope="global")` writes the DAEMON host's files. When the daemon runs elsewhere (a remote/cage daemon) or its knowledge store is isolated, it refuses with `error.global_render_authority` rather than writing files no session reads. To refresh an operator host's global files from that daemon, run `bro render global` on the operator host (`--check` to preview, `--provider` for one file): it asks the daemon for a global render plan computed against the host's `~/.blackbox/BLACKBOX.md` path and applies the managed regions locally with the usual backups and shrink guard. The host running the command is the target policy; nothing pushes global renders to hosts.

### Project render

Use when the guidance belongs only to the current repo and its project-local memory files.
Call `bbox_render(scope="project", project="<project-selector>")` from a managed
bro-harness session bound to the owning checkout. Its locality client obtains
and applies the render plan there. Direct remote MCP cannot write the caller's
checkout; `error.render_locality_required` requires this owner execution lane,
not a different daemon path or hand-authored internal transport parameters.

## What each verb is not

- `bbox_render` is not a review step. It publishes what is already approved/renderable.
- `bbox_render` is not a hot-context mechanism. If the goal is "keep this active-arc guidance visible across turns for one execution lane," use `bbox_pin`, not render.
- `bbox_absorb` performs no import or publication.
- `bbox_review` is not rendering. It changes whether pending entries are accepted.
- `bbox_lint` is not a sync step. It is hygiene/diagnostics.

## When to reach for lint

Use `bbox_lint`:

- before large knowledge-store refactors
- after bulk knowledge imports or review
- when the rendered output looks inconsistent with what you expected

## Keep hot vs cold

Keep hot in tool docs:

- render publishes
- review approves
- lint diagnoses

Keep cold here:

- forward vs reverse lifecycle
- scope thinking
- owner-side application and review of pending entries
