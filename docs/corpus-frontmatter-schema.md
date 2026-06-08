---
title: "Corpus Frontmatter Schema"
kind: design-hub
corpus: blackbox-design
topic:
  - corpus
  - frontmatter-schema
tags:
  - corpus
  - frontmatter
brief: "The frontmatter contract shared by the design/, research/, and specs/ corpora: the chassis fields every doc carries, the per-corpus field vocabularies, the topic[] convention, and the relationship between this schema and the per-corpus map documents."
---

# Corpus Frontmatter Schema

This document is the **canonical frontmatter contract** for the three
**sibling corpora** that live in the Blackbox repo:

- [`design/`](../design/design-corpus.md) — *intent*: what we will build.
- [`research/`](../research/research-corpus.md) — *description*: what the world
  does, evidence-graded.
- [`specs/`](../specs/specs-corpus.md) — *canon*: what each subsystem should
  be, normatively, source-grounded.

If you are authoring a new doc in any of those three directories, you are in
the right place. The chassis is shared; the per-corpus vocabulary is not.

> **What this doc is NOT.** It is not the corpus maps themselves. Each corpus
> owns a top-level `<corpus>-corpus.md` map that lists its hubs, lifecycle
> status, and the conventions that bind that corpus's authors. The three maps
> each carry a brief "Frontmatter schema" section that points here for the
> chassis and shows the per-corpus vocabulary inline. This doc is the
> single source of truth for the *shared* contract; the per-corpus maps are
> the single source of truth for the per-corpus vocabulary.
>
> See the rationale in [§5 Why a shared chassis doc](#5-why-a-shared-chassis-doc).

## 1. The chassis

Every `.md` file in `design/`, `research/`, or `specs/` **MUST** start with a
YAML frontmatter block delimited by `---` lines. The block lists, in this
order:

```yaml
---
title: "…"           # required, quoted string
kind: …              # required, one of the kind values per corpus (§2.1)
corpus: blackbox-…   # required, exactly matches the directory the doc lives in
topic:               # required, list of strings; see §3
  - …
  - …
---
```

Two observations:

- **`kind` + `corpus` are the discriminator pair.** `kind` tells you what shape
  the doc is (hub vs. leaf, what sub-kind of leaf). `corpus` tells you which
  directory it lives in. Together they are the Obsidian graph node identity.
- **`title` and `topic` carry the human/agent navigation.** `title` is what
  shows up in graph nodes and search hits; `topic` is the path-like tag list
  that scopes a doc to its topic home within its corpus.

The full per-corpus matrix is in [§2 Per-corpus field matrix](#2-per-corpus-field-matrix).
The `topic` list convention is in [§3 The `topic` convention](#3-the-topic-convention).
Field-by-field rules (quoting, ordering, common pitfalls) are in
[§4 Field-by-field rules](#4-field-by-field-rules).

## 2. Per-corpus field matrix

The chassis (title / kind / corpus / topic) is the same in all three corpora.
The per-corpus vocabulary — the legal values of `kind`, the lifecycle / status
lattice, the *additional* required fields — differs. The table below is the
quick reference; the full vocabulary per corpus lives in its corpus map
(linked in the rightmost column).

| | `design/` | `research/` | `specs/` |
|---|---|---|---|
| **Corpus tag** | `corpus: blackbox-design` | `corpus: blackbox-research` | `corpus: blackbox-spec` |
| **Map** | [`design/design-corpus.md`](../design/design-corpus.md) | [`research/research-corpus.md`](../research/research-corpus.md) | [`specs/specs-corpus.md`](../specs/specs-corpus.md) |
| **Hub `kind`** | `design-hub` | `research-hub` | `spec-hub` |
| **Leaf `kind`** | `design` *(130 observed)* | `research-subject`, `research-axis`, `research-finding` | `spec` |
| **Lifecycle field** | `lifecycle` | `status` | `status` |
| **Lifecycle values** | `proposed`, `partial`, `archived` *(observed `superseded` is an in-the-wild extension; see note)* | `stub`, `researching`, `enriched`, `verified` *(only `stub`/`researching`/`enriched` observed)* | `draft`, `specified`, `ratified` *(only `draft` observed)* |
| **Per-corpus extras** | `tags` *(45 leaves use it; vocabulary is open)*, `brief` | `track`, `axis`, `harness`, `subject` (research-subject only), `version`, `last_verified`, `confidence`, `platform`, `captured`, `supersedes`, `replaces`, `generated_by`, `last_reviewed` | `domain`, `spec`, `sources`, `supersedes`, `last_reviewed` |
| **Outlier `kind`** | `correction-plan` *(1 observed in `corpus: project-refactor`)* | — | — |
| **Tooling that greps it** | [`design/list-design-docs.sh`](../design/list-design-docs.sh) — only sweeps `kind: design` + `lifecycle: proposed/partial` | none observed | none observed |

**Why the columns look different in `kind`.** Each corpus grew up around a
different shape of document, and the `kind` vocabulary reflects that:

- **Design** is a single-aspect corpus (one doc = one design). Leaves are
  uniformly `kind: design`; only the `lifecycle` field tracks where the doc is
  in the proposed→partial→archived flow.
- **Research** is a **matrix** (subject × axis). Leaves split by row
  (`research-subject` = one harness at one version) and by column
  (`research-axis` = one axis across harnesses; `research-finding` = one cell
  of the matrix — a subject at a particular axis).
- **Specs** is a tree (domain → contract). Leaves are `kind: spec`; the
  `domain` and `spec` fields pick the leaf out of the tree.

### 2.1 Outliers and ambiguous vocabulary (observed, not invented)

The brief asked us to call out ambiguity rather than invent. The following
items are **observed in real files** and may or may not be official vocabulary;
treat them as exceptions, not as the rule.

- `kind: correction-plan` is observed **once**, on a single doc in
  `design/operations/whiteboards/…`, with `corpus: project-refactor` (a
  different corpus tag than the canonical `blackbox-design`). It is a
  pathology-tooling artifact, not part of the design leaf vocabulary.
  *(Observed: 1 file, `lifecycle: proposed`.)*
- `lifecycle: superseded` is observed on **5 design leaves** (e.g.
  `narf-tool-placement.md`). The corpus map and `list-design-docs.sh` only
  document `proposed` / `partial` / `archived`, and the script refuses
  anything else. The `superseded` leaves carry a `superseded_by:` pointer.
  Treat `superseded` as a *real* lifecycle value that the tooling does not
  yet understand; prefer `archived` + `superseded_by:` for new docs unless
  `superseded` is necessary for an in-flight migration.
- `status` on design leaves is **free-text**, not a controlled vocabulary.
  Observed values include `"archived"`, `"design proposal"`,
  `"partial design"`, `"implemented; archived after code audit"`, `"skeleton"`,
  `"working benchmark"`, and many more. The `lifecycle` field is the
  controlled one; `status` is a free-form annotation.
- `topics` (plural) is observed on **2 research-hub files** (`narf.md`,
  `narf-draft2.md`); every other research/design/specs doc uses the singular
  `topic:`. The schema doc itself uses the singular form. Treat `topics` as a
  typo / inconsistency unless those two docs are normative; the singular
  form is the contract.
- `research-corpus.md` and `design/design-corpus.md` both use the singular
  form for their own top-level `topic:`. The other hubs (e.g.
  `research/harness/harness-tracks.md`) also use `topic:` (singular). The
  plural form is a two-file outlier.

### 2.2 Per-corpus vocabulary detail

For the authoritative vocabulary per corpus — including the lifecycle arrows,
the source-tier grading for specs, and the version-snapshot supersession
pattern for research — see the corpus map. The maps each carry a "Frontmatter
schema" section that cross-links here.

- **Design vocabulary:** see
  [`design/design-corpus.md`](../design/design-corpus.md) § Lifecycle and the
  "Frontmatter schema" section.
- **Research vocabulary:** see
  [`research/research-corpus.md`](../research/research-corpus.md) § Conventions
  and the "Frontmatter schema" section.
- **Specs vocabulary:** see
  [`specs/specs-corpus.md`](../specs/specs-corpus.md) § Conventions (corpus-wide)
  and the "Frontmatter schema" section (already present in the seeded doc).

## 3. The `topic` convention

`topic` is a list of strings that mirrors the directory path of the doc within
its corpus, **without** the corpus root and **without** the filename. It is
how Obsidian and the rest of the tooling build the per-corpus graph
hierarchy.

**Convention:**

```yaml
topic:
  - <corpus-root-segment>      # always
  - <subdir>                   # if the doc lives in a subdir
  - <more-specific-subdir>     # deeper subdirs as needed
```

The first element is **the corpus root segment** (e.g. `corpus` for
`design/corpus/…`, `harness` for `research/harness/…`, `specs` for
`specs/bro-harness/…`). The rest of the list walks down the directory tree.

**Worked examples (observed):**

| File | Directory | `topic` (observed) |
|---|---|---|
| `design/corpus/agentic-corpus/agentic-corpus.md` | `design/corpus/agentic-corpus/` | `[corpus, agentic-corpus]` |
| `design/corpus/code-navigation/code-navigation.md` | `design/corpus/code-navigation/` | `[corpus, code-navigation]` |
| `design/orchestration/orchestration.md` | `design/orchestration/` | `[orchestration]` |
| `design/refactor-tools/refactor-tools.md` | `design/refactor-tools/` | `[refactor-tools]` |
| `design/design-corpus.md` | `design/` | `[design-corpus]` |
| `research/research-corpus.md` | `research/` | `[research-corpus]` |
| `research/harness/harness-tracks.md` | `research/harness/` | `[harness, charter]` |
| `research/harness/claude/claude-context-management.md` | `research/harness/claude/` | `[harness, claude, context-management]` |
| `research/harness/vibe/vibe-2.9.6.md` | `research/harness/vibe/` | `[harness, vibe]` |
| `specs/specs-corpus.md` | `specs/` | `[specs-corpus]` |
| `specs/bro-harness/bro-harness-spec.md` | `specs/bro-harness/` | `[specs, bro-harness, charter]` |
| `specs/bro-harness/transports.md` | `specs/bro-harness/` | `[specs, bro-harness, transports]` |

**Why it is a list and not a slash-path string.** The list shape composes with
`bbox_hybrid_search` and the Obsidian graph: a doc matches a topic query
element-by-element, and the list ordering makes the hierarchy explicit
without parsing a delimiter.

**When the file does not live in a subdir.** The list has one element, equal
to the corpus-root segment, OR a path-like shorthand. For example, the three
top-level maps use `[design-corpus]`, `[research-corpus]`, `[specs-corpus]` —
the corpus-root segment plus the `-corpus` suffix, to distinguish the map
from sibling roots.

## 4. Field-by-field rules

### 4.1 `title`

- **Required.**
- Always quoted (`"…"`). Observed in **every** doc across all three corpora.
- For docs whose filename ends in a language-specific symbol (e.g. `…/beam.md`
  in `design/refactor-tools/`) the title is the human label, not the symbol.
- For research-subject snapshots, the title conventionally ends with
  `(snapshot)` and the version is in the `version` field, not the title.

### 4.2 `kind`

- **Required.**
- Not quoted (a token, not a phrase). Unquoted scalar.
- Must be one of the legal values for the doc's corpus (§2.1).
- The discriminator from `corpus`: `corpus` says *where* (which directory
  family), `kind` says *what shape* (hub vs. leaf, what kind of leaf).

### 4.3 `corpus`

- **Required.**
- Unquoted scalar. One of: `blackbox-design`, `blackbox-research`,
  `blackbox-spec`. The single observed outlier `corpus: project-refactor`
  (§2.1) is a pathology-tooling artifact, not a sibling corpus.
- Must match the directory the doc lives in. A doc in `design/…` carries
  `corpus: blackbox-design`; a doc in `research/…` carries
  `corpus: blackbox-research`; a doc in `specs/…` carries
  `corpus: blackbox-spec`. This invariant is what
  `list-design-docs.sh` relies on (it only sweeps `kind: design` and trusts
  the directory tree to scope the corpus).

### 4.4 `topic`

- **Required.**
- A YAML list of strings. See [§3](#3-the-topic-convention) for the
  directory-mirroring convention.
- Each list element is a single token, **not** a slash-path. Use nested
  bullets (`- foo\n  - bar`) — *not* `["foo/bar"]` — to express hierarchy.

### 4.5 `lifecycle` (design only)

- **Required on `kind: design` leaves.** Not used on `kind: design-hub`.
- Unquoted scalar.
- The corpus map documents three values: `proposed`, `partial`, `archived`.
  The `list-design-docs.sh` tool only recognizes `proposed` and `partial`;
  `archived` is the third canonical value but is excluded from the tool's
  sweep by default.
- `superseded` is observed on 5 leaves and means "this design has been
  retired by a successor" (typically paired with `superseded_by:`). It is
  not a vocabulary value documented in the corpus map; treat it as an
  in-the-wild extension. See [§2.1](#21-outliers-and-ambiguous-vocabulary-observed-not-invented).

### 4.6 `status` (research, specs)

- **Required on research leaves and spec leaves.** Specs hubs do not carry
  `status`.
- Unquoted scalar (or quoted free-text — see below).
- **Research vocabulary:** `stub`, `researching`, `enriched`, `verified`
  (lifecycle order, per the harness charter). Only `stub`, `researching`,
  and `enriched` are observed in the current corpus; `verified` is the
  next-rung value the charter calls for.
- **Specs vocabulary:** `draft`, `specified`, `ratified` (lifecycle order,
  per `specs/specs-corpus.md` § Conventions). Only `draft` is observed in
  the seeded `bro-harness` domain.
- Some research docs use a free-text `status` annotation on top of the
  controlled value (e.g. a hub note may record `status: researching` to
  signal that the track is in flight). The free-text variation appears in
  the research-hub outliers (`narf.md`, `narf-draft2.md`); the controlled
  value is the contract.

### 4.7 Optional / corpus-specific fields

**Design:**

- `brief`: one-line human summary. Observed on **all hubs** and on many
  leaves. **Recommended on every doc** — it is what the Obsidian graph
  previews.
- `tags`: open-vocabulary list. Observed on **45 design leaves** and **10
  design hubs**. Vocabulary is open (e.g. `refactor-tools`,
  `code-navigation`, `java`, `rust`, `pathology`, `mcp`, `atoms`,
  `integrations`, `slack`, `obsidian`, `whiteboard`, `lsp`, `jdtls`,
  `roslyn`, `msbuild`, `beam`, `elixir`, `csharp`, `gap-notes`,
  `implemented-atoms`, …). The contract is *a list of tokens*; the
  vocabulary grows organically. A tag is conventionally a hyphenated
  lowercase noun.
- `date`, `updated`, `revision`: ISO-8601 date / human revision note.
  Optional and observed on a minority of leaves.
- `supersedes`, `superseded_by`: free-text pointer to a successor or
  predecessor doc (typically the bare filename, sometimes with rationale).
  Optional.
- `question_shapes`: structured retrieval-shape annotation. Observed once.

**Research:**

- `track`: which research track the doc belongs to. Observed value:
  `harness` (the only seeded track). **Required on research leaves.**
- `axis` (research-axis, research-finding only): the matrix column.
  Observed values: `agent-loop`, `builtin-tools`, `compaction`,
  `context-management`, `hooks`, `mcp`, `memory-persistence`, `metatools`,
  `modes-personas`, `planning-goals`, `privilege-approvals`, `robustness`,
  `session-lifecycle`, `skills`, `subagents`, `transport`. **Required on
  research-axis and research-finding leaves.**
- `harness` (research-finding only): the matrix row. Observed values:
  `claude`, `codex`, `vibe`, `antigravity`. **Required on
  research-finding leaves.**
- `version`: the harness version the leaf was mined against. **Required
  on research-subject and research-finding leaves.** Quoted string
  matching the harness's reported version (e.g. `"2.1.160"`, `"0.136.0"`,
  `"2.9.6"`, `"1.0.4"`).
- `last_verified`: when the leaf was last re-verified against the live
  harness. Same value as `version` when the leaf is current. **Required
  on research-finding leaves.**
- `confidence`: how confident the miner is in the leaf's claims. Observed
  values: `high` (53 leaves), `medium` (6), `mixed` (3). **Required on
  research-finding leaves.**
- `platform` (research-subject only): the host platform the snapshot was
  captured on. Observed: `linux-x86_64`, `macos-aarch64`.
- `captured` (research-subject only): the capture date. Observed:
  `"2026-06-02"` (all four snapshots).
- `supersedes` (research-subject only): predecessor snapshot. Observed:
  `null` (all four snapshots are first-version).
- `replaces` (research-subject only): non-version predecessor (e.g.
  antigravity replaces gemini). Observed once.
- `generated_by`, `last_reviewed`: provenance and review metadata. Used
  on the two research-hub outliers.

**Specs:**

- `domain`: the spec domain. Observed value: `bro-harness` (the only
  seeded domain). **Required on spec hubs and spec leaves.**
- `spec`: the contract key within the domain. **Required on spec leaves.**
  Observed values: `agent-loop`, `transports`.
- `sources`: list of authority strings. **Required on spec leaves.**
  Observed shapes: `"research:<path-to-research-leaf>"`,
  `"vendor:<API name>"`, `"standard:<…>"`.
- `supersedes`: predecessor spec. Optional, observed as `null` in the
  current seeded corpus.
- `last_reviewed`: ISO-8601 date. Optional.

## 5. Why a shared chassis doc

We chose a single shared chassis doc that the three corpus maps link to
(rather than three independent "Frontmatter schema" sections, one per map).
The reasons:

1. **The chassis is actually shared.** The four required fields
   (`title` / `kind` / `corpus` / `topic`) are identical in shape and
   semantics across all three corpora; only the per-corpus vocabulary of
   `kind` and the lifecycle/status lattice differ. Putting the chassis
   in one place keeps the common contract from drifting between the three
   maps.
2. **The three maps already cross-link.** Each map points to its siblings
   (e.g. `specs/specs-corpus.md` cites `design/design-corpus.md` and
   `research/research-corpus.md` in its opening). A shared chassis doc
   composes with that pattern: the chassis is the cross-cutting doc, and
   each map owns only its per-corpus vocabulary.
3. **The per-corpus vocabulary stays in the corpus map.** The maps are
   still the authoritative place for "what does `lifecycle: partial` mean
   in design", "what does `status: enriched` mean in research", and "what
   is the source-tier grading in specs". A reader who follows the link
   from a map into this doc gets the chassis, then follows the link back
   (or into the per-corpus map section) for the per-corpus detail. The
   authoritative source for the per-corpus vocabulary is the corpus map,
   not this doc.
4. **A future fourth sibling (e.g. `adr/`, `protocols/`) would re-use the
   chassis unchanged.** Centralizing the chassis means a new corpus adds a
   one-line row to the matrix in [§2](#2-per-corpus-field-matrix) instead
   of forking the schema across three places.

The compromise: each corpus map still carries a **brief** "Frontmatter
schema" section. The section is short — it points here for the chassis and
shows the per-corpus vocabulary inline as a quick reference. The
`specs/specs-corpus.md` seed already has such a section; the new
"Frontmatter schema" sections in `design/design-corpus.md` and
`research/research-corpus.md` follow the same shape.

## 6. Worked examples (one hub, one leaf per corpus)

### 6.1 `design/`

**Hub** (`design/corpus/corpus.md`):

```yaml
---
title: "Corpus"
kind: design-hub
corpus: blackbox-design
topic:
  - corpus
brief: "Hub for Blackbox corpus, knowledge, provenance, storage, code navigation, and Badgey designs."
---
```

**Leaf** (`design/corpus/agentic-corpus/reflective-project-graph.md`,
observed, kind: design / lifecycle: proposed):

```yaml
---
title: "Reflective Project Graph"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - graph
  - project-locality
---
```

### 6.2 `research/`

**Hub** (`research/harness/harness-tracks.md`):

```yaml
---
title: "Harness Research — Tracks & Charter"
kind: research-hub
corpus: blackbox-research
track: harness
topic:
  - harness
  - charter
brief: "Hub and charter for the harness research track: the study of agentic coding CLIs (…)."
---
```

**Leaf** (`research/harness/claude/claude-2.1.160.md`):

```yaml
---
title: "Claude Code — 2.1.160 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: claude
version: "2.1.160"
platform: macos-aarch64
captured: "2026-06-02"
supersedes: null
status: enriched
topic:
  - harness
  - claude
brief: "Point-in-time research snapshot for Claude Code 2.1.160: provenance plus the per-axis checklist. (…)"
---
```

**Cell** (`research/harness/claude/claude-context-management.md`,
kind: research-finding):

```yaml
---
title: "Claude - Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: context-management
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - context-management
brief: "Claude Code 2.1.160 context construction separates cache-stable defaults, (…)"
---
```

### 6.3 `specs/`

**Hub** (`specs/bro-harness/bro-harness-spec.md`):

```yaml
---
title: "Bro-Harness Spec — Domain Charter"
kind: spec-hub
corpus: blackbox-spec
domain: bro-harness
topic:
  - specs
  - bro-harness
  - charter
brief: "Hub and charter for the bro-harness spec domain: the canonical, source-grounded contracts (…)"
---
```

**Leaf** (`specs/bro-harness/transports.md`):

```yaml
---
title: "Bro-Harness Transports — Spec"
kind: spec
corpus: blackbox-spec
domain: bro-harness
spec: transports
topic:
  - specs
  - bro-harness
  - transports
status: draft
sources:
  - "research:research/harness/transport.md"
  - "research:research/harness/robustness.md"
  - "vendor:Anthropic Messages API"
  - "vendor:OpenAI Responses API"
  - "vendor:Mistral chat-completions API"
supersedes: null
last_reviewed: "2026-06-02"
---
```

## 7. Quick checklist for a new doc

1. Pick the corpus (`design/`, `research/`, or `specs/`).
2. Pick the doc shape from the per-corpus matrix in
   [§2](#2-per-corpus-field-matrix) → know your `kind` and any per-corpus
   required fields (`lifecycle` for design, `status` + `track` + (for
   findings) `axis`/`harness`/`version`/`last_verified`/`confidence` for
   research, `status` + `domain` + `spec` + `sources` for specs).
3. Set `corpus: blackbox-<name>` to match the directory.
4. Set `topic:` to mirror the directory path, minus the corpus root and
   the filename. See [§3](#3-the-topic-convention).
5. Set `title:` (always quoted) and a one-line `brief:`.
6. Run the corpus map's tooling check (for design, the
   `list-design-docs.sh` sanity check) before committing.
