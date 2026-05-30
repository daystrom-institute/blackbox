---
title: "Repo-Owned Project State"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - knowledge
brief: "Invert the system-of-record for project scope: durable project knowledge lives in-repo under .bbox/, the daemon spools it into a derived index, and project render becomes a deterministic function of the committed tree."
---

# Repo-Owned Project State

**Status:** Implemented (rev 3 design; landed on `main` 2026-05-30). See
CHANGELOG "Unreleased" for the shipped surface.
**Scope:** Where project-scoped durable knowledge (and the durable record of
project activity) physically lives, who owns it, and what the daemon's role
becomes for the project layer.
**Motivating evidence:** a second-machine bootstrap walkthrough (2026-05-29);
the issue log that prompted this design has since been resolved and removed.
**Ancestor:** [Config and Artifact Locality](../../operations/config-artifacts/config-and-artifact-locality.md)
(archived) — same half-finished locality migration, one layer down.

> **rev 2 changelog:** corrected the current-state model (project stores key on
> an absolute **path string**, not `project_id`); demoted the promotion seam and
> the spooler from "mostly exists" to net-new contracts with a known watcher bug
> to fix first; made `(repo_id, relative-root)` identity and monorepo support
> requirements rather than open questions; added the write-path and
> source-of-truth-tree sections; selected render policy B.
>
> **rev 3 changelog (cite/precision pass):** fixed the `bbox_project_rename` cite
> (`tools/projects.rs:278`, not `:151`); corrected thread storage wording (params
> take `Option<String>`, stored as a plain `String` via `unwrap_or("")`); fixed
> the promotion cite to `thread_promote` (`threads.rs:692`); resolved the
> render/source circularity with explicit authoring-render vs `render --check`
> modes; added the `.bbox/` double-indexing hazard (`project_files.rs:528`
> exempts `.bbox` from the dotfile skip) to the spooler contract.

---

## Problem

Project-scoped state is logically owned by the repo but physically owned by the
host, under a key that does not travel. That mismatch is the root of a family of
bugs.

### Current state (verified against code)

Project-scoped stores identify their project by an **absolute path string**, held
in central host JSON — not by the `project_id` hash, and not by anything
repo-anchored:

- Knowledge entries carry `project: Option<String>` (`src/knowledge.rs:393`), and
  the render scope filter matches it by **exact path equality**:
  `entry.project.as_deref() == Some(dir)` (`src/knowledge.rs:2030`). The store is
  a single JSON file written atomically (`KnowledgeStore::save`,
  `src/knowledge.rs:700`).
- Thread *inputs* accept an optional project path (params, `src/threads.rs:33`),
  stored on the thread as a plain `String` (`src/threads.rs:172`, populated from
  `p.project.unwrap_or("")`, `:328`). Notes (`src/notes.rs:32`) and pins
  (`src/pins.rs:32`) hold `project: Option<String>`. All are path strings, not ids.
- `bbox_project_rename` migrates state by rewriting `old_record.canonical_path` →
  `record.canonical_path` across the stores (`src/tools/projects.rs:278`+, via
  `migrate_project_refs` at `:285`), which is the tell that **the path string is
  the identity**, not a stable id.
- The registry *record* does derive a `project_id` from the canonicalized realpath
  and a `repo_id` from git root identity (`src/projects.rs`), but the stores above
  are not keyed on `project_id`. The registry id is bookkeeping; the path string
  is what scopes the data.

So portability is actually **worse** than "per-machine id": an absolute path
string doesn't survive a different `$HOME`, a different checkout location, or a
second user — let alone a second machine. And the repo carries only the **rendered
markdown** (`CLAUDE.md` / `AGENTS.md` / `GEMINI.md`), a **lossy** projection that
cannot be reverse-derived to the structured entries (`bbox_absorb` is now an
explicit no-op, `src/tools/render.rs:24`).

The one thing that travels with the repo (markdown) can't be reverse-derived; the
thing that should travel (structured entries) lives off-repo under a key that
doesn't move.

### What this breaks (observed)

On a fresh second machine where the daemon's store is empty-for-this-project but
the repo's instruction files are already managed and committed:

1. `bbox_bootstrap` **refuses** — detects the files as "already
   blackbox-generated" and imports nothing (HIGH#2 of the second-machine
   bootstrap issues).
2. `bbox_render scope=project` from the empty-for-this-project store produces a
   **near-empty stub**, which would **overwrite committed content** (a 74-line
   conventions file collapses to ~6 lines).
3. `bbox_absorb`, the former reverse path, is a **no-op** — rendered markdown is
   not round-trippable to structured entries.

Net: committed project knowledge can be neither re-imported nor reproduced on the
second machine. There is no supported reconciliation path. This is the
system-of-record being in the wrong place for the project scope.

## Non-Goals

- **Global scope stays host/daemon-owned.** "I always use `fd`", "prefer rustls" —
  machine/user-level, no repo to anchor to. This proposal flips only the *project*
  layer.
- **Not resurrecting `bbox_absorb`.** This removes the need to round-trip markdown
  by making the structured form the committed source; markdown becomes a derived
  view.
- **Not committing live activity.** High-churn, session-bound activity stays local
  (see the split). This is not "put threads and notes in git wholesale."
- **Not a transcript/index sync.** The tantivy index, embeddings, and edge
  sidecars remain derived and host-local; never committed.

## Core proposal: invert the system-of-record for project scope

Make the **repo** the system of record for project-scoped *durable knowledge*.
The **daemon becomes a derived index/cache** over it, not the authority.

Consequences that fall out:

- `bbox_render scope=project` becomes a **pure function of `.bbox/`** (authoring
  render reads the working tree; `render --check` reads the committed tree — see
  Source-of-truth) → deterministic, byte-identical on every machine, no clobber.
- Second-machine bootstrap becomes a **non-event**: clone → daemon spools `.bbox/`
  → render reproduces the same output.
- Losing the daemon store stops being catastrophic for projects — the project
  layer **rebuilds from the repos**. The host store becomes a cache.
- The clobber-guard and absorb-deprecation issues from the scratch doc are
  **closed by construction**, not patched.

The daemon's relationship to a project inverts from *"daemon owns project state,
repo gets a render"* to *"repo owns project state, daemon indexes it."*

## Split by nature, not by scope

"Project memories, notes, threads should live in-repo" is right for the durable
layer and wrong for the activity layer — different natures want different homes.

| Kind | Nature | Home | Why |
|---|---|---|---|
| knowledge, decisions, conventions | durable, reviewable, branch-aligned | **committed** `.bbox/` | config-like; git is ideal; PR review *is* the approval workflow |
| accepted roadmap items | durable intent | **committed** `.bbox/` | but see roadmap caveat below |
| promoted/resolved thread snapshots | durable record of past activity | **committed** `.bbox/record/` | the *record* belongs with the code it explains |
| live threads, side-channel notes | high-churn activity, often session/bro/task-bound | **local** `.bbox/local/` (gitignored) | committing churns merges and *leaks per-host identity* (session UUIDs, bro IDs, absolute paths) |
| pins | ambient execution context for one lane | **local** `.bbox/local/` | never rendered today; scoped to a live session/bro/thread — committing them is meaningless cross-machine |
| index, embeddings, edge sidecars | derived cache | **host** (`~/.local/state`) | reproducible from source; never authoritative |

**Roadmap caveat (from review):** accepted roadmap *items* are durable and can be
committed, but active *prioritization/status churn* is workflow state — keep the
ranking/`next` machinery in a local or workflow-owned lane, commit only the
accepted item bodies.

Committing live activity would create constant merge churn **and** leak per-host
identity. The codebase already reaches for this committed-vs-local boundary:
`bbox_project_init` scaffolds `.bbox/config.toml` + `.bbox/mcp.json` (committed)
alongside `.bbox/local/.gitignore`, and register-time discovery installs both
`.bbox/<kind>/*.json` (committed) and `.bbox/local/<kind>/*.json` (local) as
project artifacts (`src/artifacts.rs:986`, `:989`). We extend an existing pattern,
we do not invent one.

## Layout

```
<repo>/
  .bbox/                      # committed — project system of record
    config.toml               # (exists) project config
    mcp.json                  # (exists) project MCP wiring
    knowledge/                # NEW: one file per entry
      <entry-id>.json
    decisions/                # NEW: durable commitments w/ rationale + supersession
    roadmap/                  # NEW: accepted item bodies (not ranking state)
    record/                   # NEW: promoted/resolved thread snapshots (scrubbed)
    local/                    # gitignored — host/session-bound activity
      .gitignore              # (exists)
      threads/ notes/ pins/   # NEW: live activity
      <kind>/                 # (exists) local artifacts
  CLAUDE.md / AGENTS.md / GEMINI.md   # derived; committed behind render --check (policy B)
  PROJECT.md                  # hand-authored, included by reference (unchanged)
```

## Identity (requirement, not open question)

The project layer must stop scoping on an absolute path string. Forks, rebases,
shallow clones, and monorepos make a naive choice wrong, so this is specified, not
deferred:

- **Key the committed layer on `(repo_id, bbox_root_relpath)`**, where
  `bbox_root_relpath` is the `.bbox/` directory's path relative to the repo root.
  This makes **monorepos a first-class case** (each sub-project's `.bbox/` is a
  distinct scope) and decouples identity from `$HOME`/checkout location.
- **`repo_id` is a repo-*family* key, not a complete project key.** It derives
  from first-commit identity, so it: changes under `filter-repo`/history rewrite;
  is absent/altered under shallow clones; and **conflates a fork with its
  upstream** (shared history → shared `repo_id`). Required handling:
  - History rewrite → a `repo_id` remap recorded in `.bbox/config.toml` (an
    `aka_repo_ids` list) so the spooler reconciles old and new identity.
  - Fork divergence → the `(repo_id, relpath)` scope is shared by design; fork and
    upstream that genuinely want separate knowledge set an explicit
    `project_key_override` in `.bbox/config.toml`.
  - Shallow clone → fall back to the recorded `repo_id` in `.bbox/config.toml`
    (committed) rather than recomputing from absent history.
- The host-local layer (live activity, cache) may keep using `project_id`/path —
  it never travels, so its identity choice is unconstrained.

## Write path (new — closes the dual-authority gap)

Today `bbox_learn` and friends mutate the central JSON store directly
(`KnowledgeStore::save`, `src/knowledge.rs:700`). After the inversion that store
is no longer authoritative for project scope, so the write path must change or you
get **two systems of record disagreeing**:

- A project-scope `bbox_learn` / `bbox_decide` / `bbox_forget` **writes the
  corresponding `.bbox/<kind>/<id>.json` file** (one-file-per-entry) and lets the
  spooler re-index; it does not write the central project JSON as authority.
- The central store may keep a **read-through cache** of project entries for query
  speed, but it is invalidated/rebuilt from `.bbox/`, never the source.
- Global-scope writes are unchanged (host store remains authoritative there).

Open sub-decision: whether writes commit (`git add` the entry file) or leave it
staged/working for the human to commit. Default: leave it in the working tree,
surfaced by `bbox_lint`/status — see source-of-truth below.

## Source-of-truth tree (new)

The spooler watches files, but "verified project truth" must be tied to committed
state, not whatever is in the working tree:

- **Indexing/derivation source = the working tree** (so an agent sees its own
  just-written entries immediately). Uncommitted/unmerged `.bbox/` entries are
  indexed as **provisional**, never as verified project truth.
- **Two render modes** resolve the apparent circularity (how do you land an entry
  change *and* its matching generated markdown in one commit without an
  amend loop?):
  - **Authoring render** — local `bbox_render` reads the **working tree**, so an
    agent edits `.bbox/` entries and regenerates the markdown in the *same*
    uncommitted change set; both go into one commit.
  - **`render --check`** — CI reads the **committed tree under test** and fails if
    the committed markdown doesn't match what `.bbox/ @ that tree` renders.
- **`verified` authority derives from committed/reviewed state**, not the working
  tree — so it carries real weight only on **protected / reviewed / signed**
  branches and is advisory on a scratch local branch. The design states this
  rather than pretending a boolean field equals review.

## Spooler contract (new — fix-first, then extend)

The "extend the existing reindexer" plan has a prerequisite bug. The current
`BbxWatcher` ignores its `project_id` argument and **reconstructs a project id
from the parent directory name of `.bbox`** (`src/watcher.rs:113`). Extending this
as the knowledge spooler without fixing it would mis-scope or corrupt installs.

Contract:

1. **Fix the watcher** to use the passed project identity (`(repo_id, relpath)`),
   not a dir-name reconstruction.
2. **Generation/snapshot semantics.** Each spool pass computes the entry set from
   the current tree and **purges entries no longer present** — otherwise a branch
   switch that drops an entry leaves *ghost knowledge* in the index. (The existing
   knowledge indexer rebuilds docs from one JSON file; the repo-owned spooler needs
   the equivalent rebuild keyed to the working tree / branch.)
3. **Ordering vs the existing reindexer.** `.bbox/` spooling and `project_file`
   indexing share the same background lane; define precedence so a render triggered
   right after a checkout sees a consistent snapshot (spool `.bbox/` before
   serving project render).
4. **Cost.** One-file-per-entry means many small files; the watcher must batch and
   debounce (large `.bbox/` trees + frequent branch switches) rather than
   re-reading the whole tree per event.
5. **Avoid double-indexing `.bbox/`.** Generic `project_file` indexing currently
   does **not** skip `.bbox/` — `is_skipped_entry` explicitly exempts it from the
   dotfile skip (`src/index/project_files.rs:528`) and JSON/TOML/MD are indexable
   (`:535`). So `.bbox/knowledge/*.json` would be indexed twice — once as a raw
   project_file, once as a structured knowledge entity — producing duplicate,
   confusing search hits. The spooler must **exclude the structured `.bbox/` dirs**
   (`knowledge/`, `decisions/`, `roadmap/`, `record/`) from generic project_file
   indexing and own them as knowledge entities instead.

## Promotion contract (new — this is net-new, not "already there")

`bbox_thread promote` (`thread_promote`, `src/threads.rs:692`) today only flips
`status = Promoted`, sets `promoted_to`, saves the central store (`:719`), and
enqueues an embedding — there is **no snapshot writer, no committed record schema,
and no scrub gate.** The activity→record seam must be built:

- A **record schema** under `.bbox/record/<id>.json`: the durable, human-meaningful
  summary of the thread/investigation, with stable id and links to the knowledge
  entries it produced.
- A **scrub gate** that strips session/bro/task IDs, absolute host paths, tokens,
  and raw transcript text before anything lands in a committed file. This gate is
  shared with the write path (below) and runs at write time **and** in CI.

## Merge strategy

- **One file per entry** under `.bbox/<kind>/<id>.json` (the layout this repo's own
  `memory/` dir uses). Minimizes textual conflicts; each entry is a reviewable diff.
- **Separate files avoid textual conflicts but not *semantic* ones** — two branches
  can add contradictory conventions that merge cleanly as text. `bbox_lint` must run
  at merge / in CI to catch contradictions and duplicates; this is a required gate,
  not optional hygiene.
- **Entry IDs are UUID/content-hash**, never host-local sequence, so they survive
  rebase, cherry-pick, and cross-machine merge. Supersession is a marked edge, not a
  delete, so concurrent branches converge.

## Render policy — decided: B (commit + `render --check`)

Two candidates:

- **A. Gitignore the markdown, render on pickup.** No staleness possible, but a
  **fresh clone's agent reads `CLAUDE.md` before the daemon has rendered** —
  a bootstrap gap — and the consumed file isn't reviewable in a PR.
- **B. Commit the generated markdown behind a `render --check` CI gate.** ✅
  Chosen. Provider files are consumed before the daemon can help (including by an
  agent starting in a fresh clone), so they must exist at checkout. `.bbox/` stays
  authoritative; CI fails a stale render; the markdown is diffable in review.

The current broken third option — commit a render with no committed structured
source — is exactly what we are leaving.

## Migration path

1. **Eject** existing central project state into the repo: a one-time
   `bbox_project_eject` writes the host store's project-scope entries for `<repo>`
   into `.bbox/<kind>/<id>.json` (one-file-per-entry, scrubbed, stable IDs, recorded
   `repo_id` in `.bbox/config.toml`). Repurposes the logic bootstrap/absorb needed.
2. **Commit** `.bbox/` and the regenerated markdown (policy B).
3. The spooler ingests `.bbox/` on next reindex; central project entries become a
   read-through cache.
4. Second-machine repos with only rendered markdown are handled by ejecting once
   from whichever host still holds the entries; every clone is consistent after.

## What this closes

Mapped to the second-machine bootstrap issues (scratch log since resolved and removed):

- **HIGH#2 (clobber trap)** — gone by construction: render derives from the
  committed tree, identical everywhere.
- **HIGH#1 (`bbox_absorb` no-op vs docs)** — the need for absorb disappears;
  structured source is committed and authoritative.
- **MEDIUM (render-target staleness)** — folded into the render-policy decision and
  the ancestor locality doc's unfinished migration.

## Open questions (genuinely undecided)

- **Write-commit boundary** — does a project-scope `bbox_learn` stage the entry
  file, or auto-commit it? (Default proposed: leave in working tree.)
- **Cache vs cross-repo queries** — does the daemon store stay authoritative for
  queries that span multiple repos, or is every project query served from spooled
  per-repo indexes?
- **`verified` granularity** — per-entry field vs a branch-protection-derived "this
  ref is reviewed" signal.
- **Conflict UX** — is a contradictory merge a `bbox_lint` CI failure, a normal git
  conflict, or a structured merge driver for `.bbox/`?

## Relationship to prior art

[Config and Artifact Locality](../../operations/config-artifacts/config-and-artifact-locality.md)
(archived, rev 3) tackled the project-local *artifact* home, secret management, and
the half-finished XDG/render-target migration — and flagged the exact
`~/.claude-shared` vs `~/.blackbox/BLACKBOX.md` drift this proposal's motivating
scratch doc rediscovered. This is the knowledge/activity-store sequel: same
locality principle, applied to the stores that currently sit furthest from the code
they describe.
