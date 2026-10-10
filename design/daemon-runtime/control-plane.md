---
title: "Task control plane"
kind: design
corpus: blackbox-design
topic:
  - daemon-runtime
brief: "Headless task control, roster observation and managed-worktree closeout."
---

# Task control plane

The daemon owns task state and execution admission. HTTP control routes and
MCP tools call the same dispatch, resume, steer, interrupt, cancel and status
implementations. Clients communicate over HTTP and do not own a parallel task
store or worker lifecycle.

## Dispatch identity and configuration

`/control/exec` and `/control/resume` create tasks with origin `control`.
MCP `bro_exec` and `bro_resume` create tasks with origin `agentdispatch`.
Origin overrides are internal fields that caller JSON cannot deserialize.
Stored `cockpit` origins remain readable and retain their control-path policy.

Control tasks use the `interactive` MCP surface and may receive operator MCP
servers from `fleet.json`. Legacy cockpit tasks use the same policy. Other
origins retain their own filtered surfaces and do not receive these servers.
The `bro-config` crate reads connection settings, opaque MCP definitions and
per-project dispatch environment settings without an execution runtime.

Provider execution follows the provider lane. Claude-compatible providers run
as CLI children; Codex runs through app-server; harness-backed providers run in
a standalone `bro-harness` process. The executor owns worker control and durable
event logs. The daemon persists its task record and event receipt mirror.

## Observation

`/control/status/{task_id}` and `/control/dashboard` expose task state.
`/control/roster` returns a versioned snapshot; `/control/roster/stream` publishes
sequence-ordered deltas and requests a resync when the stream cannot preserve
continuity. The snapshot includes daemon build identity for diagnostics.

Roster rows are bounded summaries. Transcript coordinates name the daemon's
receipt file, not a file on an arbitrary caller's host. Exact result reads use
the task status detail API. `/tail` supplies the event stream used by `bro tail`.

Task status follows worker completion. A provider turn result alone does not
prove the worker has exited. Recovery reattaches eligible workers and keeps
publishing roster updates through completion.

## Managed-worktree closeout

`/control/closeout` accepts a typed request and returns a structured phase
outcome. The daemon translates protocol types to the worktree driver's internal
types. Both this endpoint and the harness worktree tool use shared preparation
and closeout logic.

The request selects a worktree and disposition. Mutating dispositions require
`confirm=true`; publishing also requires a commit message. Preparation checks
managed roots, eligible branch prefixes and detached HEAD before mutation.
Existing managed-worktree roots remain recognized so their metadata and
closeout protection remain valid.

An explicit target wins. Otherwise the endpoint resolves the worktree's stored
fork branch, then the base checkout's current branch, then `main`. Git operations
are anchored on the requested worktree, not the daemon's working directory.

`dry_run=true` performs readiness checks for the requested disposition without
running mutation phases. Callers may supply resolved closeout hooks, which are
translated to driver policy and skipped during dry runs. Blocking Git and hook
operations run on the blocking pool. Candidate integration uses the project's
merge gate before moving the base branch.

## Authentication

The configured MCP bearer gate protects all `/control/*` routes, including
roster streams. Public health and readiness probes remain separate. Enabling
this gate does not put `/tail` behind it.
