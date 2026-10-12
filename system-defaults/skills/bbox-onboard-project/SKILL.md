---
name: bbox-onboard-project
description: Register a checkout with Blackbox, commit its returned identity files, and verify source publication. Use when the user asks to register, onboard, enroll or add a project or repo to Blackbox, or when a Blackbox tool reports that the current path is not a registered project.
---

# Onboard a project to Blackbox

Registration is an operator operation on the `ops` surface. Run the calls below from a shell with `bro`, or ask the operator to run them when `bro` is not available to you. The path you register is the absolute path of the checkout on the host that owns it, which is not necessarily a path the daemon can read.

## 1. Check whether it already exists

```text
bbox_project_list()
bro mcp call bbox_project_catalog_list '{"query":"<project name>"}' --surface ops
```

`bbox_project_list` shows attached roots only, so a project published from another host can be missing there and still exist in the catalog. `error.project_catalog_inactive` from the catalog call means this deployment has no active catalog: `bbox_project_list` is then the whole answer, and step 4 verifies through it instead of the catalog tools.

Reuse a matching project instead of creating a second one.

## 2. Register

```bash
bro mcp call bbox_project_register '{"path":"<absolute checkout path>"}' --surface ops
```

When the daemon cannot see the path, it routes enrollment to the checkout-host collector whose enroll root most specifically contains it, and that collector scaffolds `.bbox` in the checkout. No per-project daemon or collector config edit is needed. Repeating the call with the same path reuses pending enrollment.

Add `"producer":"<producer id>"` only to resolve `error.project_onboarding_ambiguous`, choosing a returned candidate whose host owns the path. Guessing a producer up front can bind the project to the wrong host.

A receipt that reports enrollment as pending is not a completed enrollment. Read what the receipt says happened.

If you do not know which host or producer applies, read the MCP resource `blackbox://skills/onboard-project/SKILL.md` when your client can read resources. The daemon renders its current checkout hosts, enroll roots and producer policy there.

## 3. Commit the identity files

When the receipt reports `identity_committed: false`, commit exactly the returned `commit_paths` on the returned `published_ref` in that checkout. Stage those paths explicitly and leave other edits and index entries alone, because the checkout may hold someone else's uncommitted work. Do not push unless the user asks: the collector reads the local ref.

Nothing is published until this commit exists. If your assignment forbids commits, or the ref cannot be updated safely, stop and report the identity commit as the remaining step.

## 4. Verify

With an active catalog:

```bash
bro mcp call bbox_project_catalog_list '{"query":"<project name>"}' --surface ops
bro mcp call bbox_project_publisher_status '{"project_id":"<project_id>"}' --surface ops
```

Without one, confirm the root appears in `bbox_project_list()`.

Publication follows the collector's next pass after the identity commit. Catalog admission, source delivery, publication and index activation are separate stages, and a large repository's first index takes a while. When indexed availability is part of the request, confirm it with a known symbol or phrase:

```text
bbox_hybrid_search(query="<known symbol or phrase>", project="<project>", doc_type="project_file", limit=5)
```

Report the project identity, the identity commit, and which stage the project has reached. Keep admission, delivery, publication and search evidence distinct instead of reporting "registered" for all of them.

## When registration cannot proceed

These need the operator. State the specific missing piece and ask; do not guess credentials or edit daemon policy.

- No fresh collector covers the path: a collector must be installed on that host with an `enroll_roots` entry containing the project, and its token minted.
- No producer may claim the scope: the operator grants it in the daemon's producer config.
- A collector exists but routing failed: the operator can enroll directly on that host with `bbox-code-collector --config <collector config path> add <path>`.

Enrollment is live-reloaded, so none of this needs a service restart. If a restart of a shared service does become necessary, name the service and get approval for it first.
