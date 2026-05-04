# Whiteboards — multi-agent deliberation boards

Whiteboards are the shared deliberation surface for agents participating in a workflow or an external review. They are not a replacement for `bbox_thread`; use threads for durable work-item state and whiteboards for phase-based claims, challenges, corroboration, validation, and votes during a deliberation.

## Core Model

- A board has phases such as blind, read, validate, debate, resolve, and archived.
- Posts carry structured intent: proposal, claim, concern, or informational.
- Annotations attach to existing posts: challenge, corroborate, resolve, or validation.
- Votes summarize stance when the facilitator needs a decision signal.
- Phase transitions can wake workflow arcs waiting on `wait_for_phase`.

## When To Use

Use whiteboards when several agents need to contribute independently, then read each other's claims, challenge weak points, validate concrete assertions, and converge on a decision. Use normal `bro_*` dispatch when a single helper or one-shot fanout is enough.

In workflow specs, an ensemble node with a `board` field can post member outputs to the board. A facilitator step can inspect `whiteboard_state`, transition phases, and decide whether the arc should continue, revise, or terminate.

## Retrieval

Use `whiteboard_state` before acting so you see the current phase and existing posts. Use `whiteboard_post` for new claims or proposals, `whiteboard_annotate` for responses to existing posts, `whiteboard_vote` for verdict signals, and `whiteboard_transition` only when you are explicitly the facilitator or operator.
