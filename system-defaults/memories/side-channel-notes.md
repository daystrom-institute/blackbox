+++
title = "Side-channel notes — what to emit and why"
tags = ["notes", "bbox_note", "orchestrator", "executor", "done", "workflow", "runbook"]
order = 17
template = false
+++
# Side-channel notes — what to emit and why

`bbox_note` is not a diary. It is a structured signal channel that lets the orchestrator inspect a workstream without re-reading the whole transcript.

## What good note traffic looks like

Sparse, high-signal, and legible out of context.

The orchestrator should be able to skim the note list and answer:

- what assumptions were made?
- what broke?
- what needs follow-up?
- what got finished?

## Kinds

### `assumption`

Use when you resolve ambiguity yourself and that choice may matter later.

### `surprise`

Use when reality diverged from the expected model.

### `blocked`

Use when progress stops on something external or risky. Include the blocker, not just the feeling.

### `followup`

Use for real out-of-scope work that should not disappear.

### `learned`

Use for facts you discovered about the codebase or environment during work.

Not for user-stated rules. Those go to durable memory.

### `done`

Emit one when you are the dispatched executor for a bro/task/workflow/atom. This
includes work launched through `bro_*` MCP tools, `bro_agent_dispatch`,
workflow actors, atom invocations, and team broadcasts. The done note is the
orchestrator's fastest acceptance signal for delegated work.

Do not emit `done` merely because you performed a direct operator-side MCP
lookup, inbox/status probe, cancellation, note resolution, or timing report.
Those are controller actions unless they are themselves running inside a
dispatched executor context.

## What to keep out

- stylistic commentary
- step-by-step narration of obvious actions
- duplicate notes that restate the same thing in different wording

## Hot/cold split

The only always-hot invariant worth rendering is: dispatched executors emit
`done`, and all notes stay high-signal.

The rest belongs here.
