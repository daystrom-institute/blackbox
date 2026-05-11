# Blackbox Knowledge Store — Design v2

## Problem

User-facing operational context is fragmented across provider-specific silos:
- Claude Code reads `CLAUDE.md` + has its own "memory" feature (invisible to other providers)
- Codex CLI and Vibe both read `AGENTS.md` (currently symlinked to CLAUDE.md — wrong content)
- Gemini CLI reads `GEMINI.md` (doesn't exist yet)
- External tools (RTK, etc.) modify these files independently

No single source of truth. No way to share knowledge across providers. Manual maintenance
guarantees drift.

## Non-Goals

Not a replacement for daystrom's epistemology graph. The graph tracks architectural decisions,
specifications, code intelligence — the project's self-knowledge. The knowledge store tracks the
user's operational surface — preferences, steering, workflow memory. Orthogonal concerns.

## Architecture: Three Layers

Knowledge lives in three distinct layers, collapsed at render time into per-provider artifacts.

```
┌─────────────────────────────────────────────┐
│              blackbox (global)               │
│  ~/.claude-shared/blackbox-knowledge.json    │
│                                              │
│  Layer 1: Provider Steerage                  │
│    Claude: hooks, depth maintenance,         │
│      autonomy boundaries, tool behaviors     │
│    Codex: sandbox model, approval policies,  │
│      shell constraints, delegation rules     │
│    Gemini: foundational mandates, search     │
│      strategy, sub-agent guidance            │
│    Vibe: tool preferences, output limits     │
│                                              │
│  Layer 2: Shared Memory                      │
│    User profile, cross-project conventions,  │
│    tool awareness, workflow rules,           │
│    session observations that should persist  │
│    (replaces Claude-specific memory)         │
└──────────────┬──────────────┬───────────────┘
               │              │
               ▼              ▼
         ┌─────────────────────────┐
         │     render pipeline     │
         │                         │
         │  steerage(provider)     │
         │  + memory(provider)     │
         │  + PROJECT.md           │
         │  → CLAUDE.md / etc      │
         └────────────┬────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────┐
│              repo (version-controlled)       │
│                                              │
│  PROJECT.md    ← human-authored, shared,     │
│                  provider-neutral project     │
│                  details (build, arch, test)  │
│                                              │
│  CLAUDE.md     ← generated artifact          │
│  AGENTS.md     ← generated artifact          │
│  GEMINI.md     ← generated artifact          │
└─────────────────────────────────────────────┘
```

### Layer 1: Provider Steerage

Behavioral instructions specific to one provider. Stored in blackbox with `providers` field set.
Never rendered into another provider's file. Examples:
- "Claude: use hooks for automated behaviors, not prompt instructions"
- "Codex: prefer apply_patch over write_file for edits"
- "Gemini: use 'Foundational Mandates' heading for critical instructions"

### Layer 2: Shared Memory

Provider-neutral knowledge about the user, their preferences, and cross-cutting concerns. Stored
in blackbox with `providers` empty (= all). Rendered into every provider's file. Examples:
- "Expert in .NET, terse communication style"
- "Never kill processes by port — use precise pkill"
- "Use blackbox MCP for transcript search, bro for multi-provider orchestration"
- "We decided to use Wolverine for messaging in daystrom" (project-scoped memory)

### Layer 3: PROJECT.md

Human-authored, version-controlled, provider-neutral. The project's "readme for agents." Build
commands, architecture overview, code conventions, test instructions. This is the one file a
teammate without blackbox would also write. Not generated, not managed by blackbox — just
included verbatim in every rendered output.

## Storage

### Global Knowledge

`~/.claude-shared/blackbox-knowledge.json` — flat JSON file. Small dataset (hundreds of entries),
needs human readability for debugging, primary access is full-scan rendering.

### Entry Schema

```json
{
  "id": "e1a2b3c4",
  "title": "Indentation standard",
  "content": "4 spaces for C#, 2 spaces for XML/JSON/YAML. No tabs.",
  "variants": {
    "codex": "Use 4-space indent for C#, 2-space for XML/JSON/YAML. Apply via apply_patch."
  },
  "category": "convention",
  "scope": "global",
  "project": null,
  "providers": [],
  "priority": "standard",
  "weight": 100,
  "status": "active",
  "approval": "user_confirmed",
  "supersedes": null,
  "expires_at": null,
  "source": "user",
  "created_at": "2026-04-14T22:00:00Z",
  "updated_at": "2026-04-14T22:00:00Z"
}
```

| Field | Type | Description |
|---|---|---|
| `id` | string | Short random ID (8 hex chars) |
| `title` | string | Stable human-readable handle. Used for dedup and display. |
| `content` | string | Default rendering text. Markdown supported. |
| `variants` | map<provider, string> | Provider-specific wording. Overrides `content` when rendering for that provider. |
| `category` | enum | `profile`, `convention`, `steering`, `build`, `tool`, `memory`, `workflow` |
| `scope` | enum | `global` or `project` |
| `project` | string? | Project path (canonical). Null for global entries. |
| `providers` | string[] | Empty = all providers. Non-empty = only these providers. |
| `priority` | enum | `critical`, `standard`, `supplementary` |
| `weight` | u32 | Explicit ordering within priority tier. Lower = rendered first. Stable across renders. |
| `status` | enum | `active`, `draft`, `superseded`, `disabled` |
| `approval` | enum | `user_confirmed`, `agent_inferred`, `imported` |
| `supersedes` | string? | ID of entry this replaces. Forms replacement chains. |
| `expires_at` | ISO 8601? | Auto-disable after this time. Null = permanent. |
| `source` | string | Who created it: `user`, `claude`, `codex`, `gemini`, `vibe`, `imported` |
| `created_at` | ISO 8601 | |
| `updated_at` | ISO 8601 | |

### Categories

- **profile** — User identity, expertise, communication style. Global, all providers.
- **convention** — Code standards, naming, formatting. Global or per-project.
- **steering** — Provider-specific behavioral instructions. Scoped to specific providers.
- **build** — Build/test/lint commands. Per-project.
- **tool** — Tool awareness and usage instructions. Usually global.
- **memory** — Observations from sessions that should persist. Replaces Claude-specific memory.
- **workflow** — Process rules and operational constraints. Usually global.

### Approval States

- **user_confirmed** — User explicitly created or verified this entry.
- **agent_inferred** — An agent created this via `blackbox_learn`. Not yet verified by user.
  Rendered with a subtle marker so the user knows it's unverified.
- **imported** — Absorbed from an external file modification (see Absorption). Treated as
  unverified until confirmed.

## MCP Tool Surface

### `blackbox_learn`
Add or update a knowledge entry.
```
blackbox_learn(
  content: string,             // required
  category: string,            // required
  title: string?,              // recommended — generated from content if omitted
  scope: "global" | "project",
  project: string?,            // inferred from cwd if scope=project
  providers: string[],         // empty = all
  priority: "critical" | "standard" | "supplementary",
  weight: u32?,                // default: 100
  variants: map?,              // provider-specific wording
  expires_at: string?,
  id: string?                  // if provided, updates existing entry
)
→ { id, status, rendered_count }
```
Triggers immediate re-render of affected files.
Agent-sourced entries get `approval: agent_inferred` automatically.

### `blackbox_knowledge`
List/search entries with filters.
```
blackbox_knowledge(
  category: string?,
  scope: string?,
  project: string?,
  provider: string?,           // entries visible to this provider
  status: string?,             // default: active
  approval: string?,
  query: string?,              // full-text search within content
  limit: int?
)
→ [{ id, title, content, category, ... }]
```

### `blackbox_forget`
Remove or supersede an entry.
```
blackbox_forget(
  id: string,
  superseded_by: string?       // if provided, marks as superseded instead of deleting
)
```

### `blackbox_render`
Regenerate provider markdown files.
```
blackbox_render(
  provider: string?,           // specific provider or all
  project: string?,            // specific project or current cwd
  dry_run: bool                // preview without writing
)
→ { files_written: [...], absorbed: [...] }
```
Runs absorption before rendering (see below).

### `blackbox_absorb`
Explicitly absorb external changes from rendered files.
```
blackbox_absorb(
  project: string?             // specific project or current cwd
)
→ { absorbed: [{ file, diff_summary, entries_created }] }
```

### `blackbox_lint`
Health check.
```
blackbox_lint()
→ { contradictions, stale, expired, unverified, orphaned }
```

## Rendering Pipeline

### Target Files

| Provider | Project file |
|---|---|
| Claude | `{project}/CLAUDE.md` |
| Codex + Vibe | `{project}/AGENTS.md` |
| Gemini | `{project}/GEMINI.md` |

AGENTS.md is a converged file for Codex and Vibe — necessary evil, least friction. Rendered as
the intersection of entries visible to either provider. Provider-specific steerage for codex or
vibe is kept minimal in this file; heavy steering goes in blackbox entries that only render when
a provider-specific file becomes available.

Claude also has a global file at `~/.claude/CLAUDE.md` — rendered from global-scoped entries only.

### Render Algorithm

```
render(provider, project):
  # 1. Absorb external changes first (see Absorption)
  absorb(project)

  # 2. Load PROJECT.md (human-authored, verbatim include)
  project_md = read("{project}/PROJECT.md") or ""

  # 3. Load entries from knowledge store
  entries = load_all_entries()
    .filter(status == "active")
    .filter(not expired)
    .filter(providers.is_empty() || providers.contains(provider))
    .filter(scope == "global" || project == entry.project)

  # 4. Group and order
  critical  = entries.filter(priority == "critical").sort_by(weight)
  standard  = entries.filter(priority == "standard").sort_by(category, weight)
  supplementary = entries.filter(priority == "supplementary").sort_by(weight)

  # 5. Resolve content (use variant if available)
  for entry in all_entries:
    text = entry.variants.get(provider) or entry.content

  # 6. Assemble
  md  = generated_header()
  md += render_critical(provider_heading(provider), critical)
  md += project_md
  md += render_by_category(standard)
  md += render_supplementary(supplementary)

  # 7. Budget enforcement (provider-specific)
  if over_budget(provider, md):
    md = trim(md)  # drop supplementary first, then oldest standard

  return md
```

### Provider-Specific Framing

| Provider | Critical heading | Notes |
|---|---|---|
| Claude | `## Standing Orders` | Matches existing CLAUDE.md convention |
| Codex | `## Critical Instructions` | |
| Vibe | `## Critical Instructions` | Shared with Codex via AGENTS.md |
| Gemini | `## Foundational Mandates` | Triggers high-priority weighting in Gemini |

### Budget Limits

| Provider | Soft limit | Notes |
|---|---|---|
| Claude | 50K chars | 1M context, generous budget |
| Codex | 15K chars | More constrained context |
| Vibe | 10K chars | ~1500 lines before attention degrades |
| Gemini | 30K chars | Large context but "lost in middle" applies |

## Absorption: Git-Based Bidirectional Sync

Rendered files are in the repo. External tools (RTK, agents, humans) may modify them. Blackbox
must detect and absorb these changes before re-rendering, or it will overwrite them.

### Mechanism

Git is the manifest. No shadow copies, no hash tracking.

```
absorb(project):
  for each rendered file (CLAUDE.md, AGENTS.md, GEMINI.md):
    # Get diff of file since last blackbox render commit
    diff = git_diff(file, since=last_render_tag_or_commit)

    if diff.is_empty():
      continue  # no external changes

    # Parse added lines into candidate entries
    candidates = parse_additions(diff)

    # Ingest as imported/unverified entries
    for candidate in candidates:
      blackbox_learn(
        content: candidate.content,
        category: infer_category(candidate),
        approval: "imported",
        source: "imported",
        ...
      )

    # The subsequent render will include these entries,
    # producing a clean file that contains the absorbed content
```

### After Render

Tag or record the commit so the next absorption cycle knows what "last render" was.
Could be a lightweight git note, a marker file, or just tracking the commit hash in
blackbox-knowledge.json itself.

### Parse Heuristics for Absorption

Added sections (new `##` headings with content) → one entry per section.
Added lines within existing sections → append to the entry that section maps to,
or create a new entry if no mapping exists.
Removed content → flag for review (don't auto-delete entries).

Complex diffs (rewrites, moves) → flag as "manual review needed" rather than
attempting surgical parsing.

## Migration Path

1. Factor current `CLAUDE.md` into `PROJECT.md` (provider-neutral ~80%) and blackbox entries
   (provider-specific steering, memory, preferences).
2. Import Claude memory entries into blackbox with `approval: imported`.
3. Create initial provider steerage entries for each provider.
4. First render → diff against current files to verify equivalence.
5. Delete AGENTS.md symlink, let renderer create it.
6. Create GEMINI.md via renderer.
7. Add `PROJECT.md` to repo. Generated files can be committed (with header) or gitignored.

## Resolved Decisions

- **AGENTS.md sharing**: Converged file for Codex + Vibe. Least friction, necessary compromise.
- **Claude memory**: Replace. Dual sources of truth = guaranteed drift.
- **Storage format**: JSON. Small dataset, human-debuggable, tool-friendly.
- **Sort order**: Explicit `weight` field, not `updated_at`. Stable rendering, no diff churn.
- **Entry size**: Soft limit ~2KB per entry. Larger = probably a document, belongs in PROJECT.md.
- **Absorption mechanism**: Git diff. No shadow copies, no manifests. Git is the manifest.
- **Render trigger**: Immediate on learn/forget. Periodic background not needed if git-based
  absorption handles external changes on next render.

## Open Questions

1. **Absorption parse quality**: How sophisticated should diff → entry parsing be? Simple
   heuristic (section-level) vs LLM-assisted (more accurate, but adds latency and cost)?

2. **Global CLAUDE.md**: Should blackbox render `~/.claude/CLAUDE.md` (global user instructions)?
   This is currently hand-maintained with profile, preferences, tool awareness. Natural fit for
   blackbox, but it's a sensitive file — bad render = broken for all projects.

3. **PROJECT.md adoption**: What happens in repos that don't have PROJECT.md? Render without it
   (steerage + memory only)? Or create a minimal one from existing CLAUDE.md content?

4. **Render commit strategy**: Should blackbox auto-commit rendered files, or just write them and
   let the user commit? Auto-commit is convenient but creates noise in git log.
