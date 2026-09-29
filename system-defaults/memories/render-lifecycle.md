+++
title = "Render lifecycle: learn, render"
tags = ["render", "knowledge", "lifecycle", "runbook"]
order = 20
template = false
+++
# Render lifecycle: learn, render

The render path is easy to misuse because the knowledge verbs are adjacent in the UI but operate on different parts of the lifecycle.

This is the compact model:

- `bbox_learn` creates or updates knowledge entries. It is the only write lane, and entries are approved with the operator before the write.
- `bbox_render` publishes renderable knowledge (entries with `render=true`) into managed files.
- Rendered files are unidirectional projections. Nothing imports edits from them.

## Normal forward path

1. create or update entries with `bbox_learn`
2. `bbox_render`
3. agents consume the managed output

This is the default path when the source of truth is the knowledge store.

## Reverse path after manual edits

Managed regions are regenerated from knowledge. To retain an intentional edit,
update its source entry with `bbox_learn`, then render again. Rendered files are
never imported. To turn existing instructions into entries, discover indexed
instruction references with `bbox_hybrid_search`, then expand with
`bbox_inspect_entity`, or read missing source through the checkout owner's file
tools. Missing indexed references do not establish absence. Propose entries for
operator approval before saving them with `bbox_learn`; there is no automatic
import lane.

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

- `bbox_render` is not an approval step. It publishes what is already stored and renderable.
- `bbox_render` is not a hot-context mechanism. If the goal is "keep this active-arc guidance visible across turns for one execution lane," put it in the dispatch brief or the work-item thread, not render.
- `bbox_learn` with `render=false` is not a render input. Those entries are indexed for recall only.

## Keep hot vs cold

Keep hot in tool docs:

- learn writes
- render publishes

Keep cold here:

- forward vs reverse lifecycle
- scope thinking
- owner-side application of render plans
