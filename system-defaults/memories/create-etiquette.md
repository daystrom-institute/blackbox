+++
title = "Create etiquette — list before create"
tags = ["create", "dedupe", "list", "knowledge", "threads", "bro", "runbook"]
order = 6
template = false
+++
# Create etiquette — list before create

Duplication bugs in Blackbox are usually not parser bugs. They are workflow bugs: creating new objects without checking whether the object already exists in a slightly different spelling or scope.

## Rule

Before any create/open/save/add action that could duplicate an existing object, call the list/get/search variant first.

## Applies to

- brofiles
- teamplates / teams
- MCP server registrations
- work threads
- dedupe-sensitive knowledge writes

## Why this matters

The storage layers are not all keyed the same way:

- some are exact-name lookups
- some are semantic or topic-sensitive
- some are scope-sensitive (global vs project)

That means create first, reconcile later is how duplicate state accumulates.

## Practical examples

- `bro_brofile(action="list")` before `bro_brofile(action="create", ...)`
- `bro_team(action="list_templates")` before `save_template`
- `bro_team(action="list")` before `create`
- `bro_mcp(action="list")` before `add`
- `bbox_thread_list(...)` before `bbox_thread(action="open", ...)`
- `bbox_knowledge(query="...")` before `bbox_learn(...)` or `bbox_decide(...)`

## What "existing match" means

Not just exact name equality.

Also check for:

- same topic under a slightly different title
- same object in project scope vs global scope
- same intent represented as an older decision that should be superseded rather than duplicated

## Hot/cold split

The short rule belongs in rendered memory: list before create.

The detailed examples and rationale belong here, because they only matter when an agent is about to create something.
