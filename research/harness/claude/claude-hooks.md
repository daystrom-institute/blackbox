---
title: "Claude · Hooks"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: hooks
version: "2.1.160"
last_verified: "2.1.160"
status: researching
confidence: mixed
topic:
  - harness
  - claude
  - hooks
brief: "How Claude Code 2.1.160 exposes hooks: configured in settings.json, fired by the harness at lifecycle points, capable of blocking/rewriting tool calls (pre-tool), with hook output treated as user feedback in context. Documented from observable behavior (a live RTK pre-tool hook transparently rewrites shell commands this session) and the harness's own framing; the full event catalogue and payload schema remain to be mined."
---

# Claude · Hooks

> **Provenance.** Mixed. The harness states hook semantics in-band (*"Hooks may
> intercept tool calls; treat hook output as user feedback"*) — **high** — and a
> live pre-tool hook is observable this session (RTK rewrites every shell command
> transparently) — **high** for that instance. The complete event catalogue and
> payload schemas are **not** observable in-band and are left open. See
> [snapshot](claude-2.1.160.md).

See the axis: [Hooks](../hooks.md).

## What is observable (high)

- **Configuration surface.** Hooks live in `settings.json` (the `update-config`
  skill is dedicated to this); they are harness-executed, not model-executed —
  *"the harness executes these, not Claude"* — which is why automated "whenever
  X" behaviors require a hook, not a memory/preference.
- **Blocking / rewriting pre-tool hook.** A live example: the RTK hook rewrites
  shell commands before execution (`git status` → `rtk git status`)
  transparently, at 0 token overhead. This confirms pre-tool hooks can **mutate**
  a tool call's arguments, not merely observe.
- **Output → context.** The harness instructs the agent to *"treat hook output as
  user feedback"* — hook stdout re-enters context as if from the user. A denied
  call is surfaced so the agent can adjust rather than retry verbatim.
- **Permission interaction.** Hooks compose with the permission mode; a denied
  call *"means the user declined it — adjust, don't retry verbatim."*

## Open

<!-- TODO(mine): the full event catalogue (PreToolUse, PostToolUse,
SessionStart/Stop, UserPromptSubmit, PreCompact, Notification, Stop, …); the
exact JSON payload each event hands the hook; matcher syntax (tool-name globs);
precedence/ordering across multiple hooks; timeout and error behavior; the
exit-code/JSON protocol a hook uses to allow/deny/modify. settings.json hook
schema. -->

## Feeds

`design/bro-harness/bro-harness-hooks.md` (the harness hook seam),
`design/bro-harness/backlog-hooks-catalog-metadata.md`.
