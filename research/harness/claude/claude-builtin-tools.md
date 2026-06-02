---
title: "Claude · Built-in Tools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: builtin-tools
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - builtin-tools
brief: "How Claude Code 2.1.160 designs its built-in tool surface: the tool inventory, the shape of each surface (Read line-numbering, Edit's read-before-edit gate + unique-match contract, Bash's dedicated-tool steering), and the verbatim tooldoc steering language that routes the agent between tools and away from anti-patterns. The highest-value cell for bro-harness's steer-without-bloat goal; backfilled from direct observation of the running harness."
---

# Claude · Built-in Tools

> **Provenance.** Direct observation of the running 2.1.160 harness — the tool
> schemas and descriptions quoted here are the agent-facing tooldocs as the model
> receives them. **confidence: high** for quoted language and surface shape;
> internal implementation is not in scope. See [snapshot](claude-2.1.160.md).

See the axis: [Built-in Tools](../builtin-tools.md).

## Inventory (families)

- **File:** `Read` (text/image/PDF/notebook), `Edit` (exact-match replace),
  `Write` (create/overwrite), `NotebookEdit`.
- **Shell:** `Bash` (timeout, `run_in_background`, sandbox override).
- **Search:** `Glob`, `Grep` (surfaced as dedicated tools, steered *over* shell).
- **Planning / delegation:** `TaskCreate`/`TaskUpdate`/… (todo), `Agent` (subagents).
- **Web:** `WebFetch`, `WebSearch`. **Skill:** `Skill`. **Deferred-tool loader:** `ToolSearch`.

## Shape of the surface (high)

- **`Read`** returns `cat -n` format, line numbers from 1; reads ≤2000 lines by
  default; supports partial reads (`offset`/`limit`) and a PDF `pages` arg.
  Steers against redundant reads: *"Do NOT re-read a file you just edited to
  verify — Edit/Write would have errored if the change failed."*
- **`Edit`** enforces a **read-before-edit gate** (*"You must Read the file in
  this conversation before editing, or the call will fail"*) and a **unique-match
  contract** (*"`old_string` … must be unique — the edit fails otherwise"*), with
  `replace_all` as the escape hatch. Surface design *forces* a safe sequence.
- **`Write`** overwriting an unread file fails — same read-gate philosophy.
- **`Bash`** carries the most steering (below); `timeout` ms-capped; `cd` warned
  against (permission-prompt trigger); interactive flags blocked.

## Tooldoc steering language (the prize — high)

The harness spends tokens on **negative guidance and cross-tool routing**, the
idiom bro-harness should adopt (minimal steer, not transcription):

- Route off shell onto dedicated tools: *"Avoid using this tool to run `cat`,
  `head`, `tail`, `sed`, `awk`, or `echo` … use the appropriate dedicated tool."*
- Global preference statement: *"Prefer the dedicated file/search tools over
  shell commands when one fits."*
- Safety-gated git: *"Commit or push only when the user asks. If on the default
  branch, branch first."*
- Anti-redundancy: the no-re-read-after-edit rule above.
- Descriptions encode *when to use* with concrete before/after examples (e.g.
  Bash `description` field examples mapping `git status` → "Show working tree
  status").

## Open

<!-- TODO(mine): string-mine the binary for the full verbatim tooldocs of every
built-in tool (esp. Grep/Glob, WebFetch, NotebookEdit) and the exact wording of
the parallel-call and sandbox guidance. Catalogue the complete negative-guidance
set and measure the minimal subset that moves behavior. -->

## Feeds

`design/bro-harness/bro-harness-tool-surface.md` — the steering idioms above are
the primary source for bro-harness's tooldoc language.
