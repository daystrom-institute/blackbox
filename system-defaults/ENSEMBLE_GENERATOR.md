# Ensemble Generator — Interactive Setup Assistant

This document is a **protocol**, not a static recipe. It defines the
conversation shape an agent follows to guide an operator through creating a
bespoke review ensemble tailored to their target repo. Read it fully before
acting; it contains the invariant dimensions, the deliberation protocol, the
recon checklist, the interview questions, and the artifact specification.

## What This Produces

A review ensemble is a set of 6 installable artifacts:

1. **Workflow JSON** — the node graph (Setup → BlindPost → Validate → Debate →
   Synthesize → WriteOutput) dispatched via `bro_orchestrate_run`.
2. **5 lens brofiles** — one per review dimension (read-only, provider-pinned,
   each owning one orthogonal question).
3. **Validator brofile** — independent evidence-prover that runs in the
   validate phase and posts confirmed/refuted/inconclusive per finding.
4. **Facilitator brofile** — orchestrator that distributes context and
   synthesizes the final review.
5. **Panel teamplate** — the 5-member team definition matching lens aliases.
6. **Gate packet** — participation rules for whiteboard phase transitions.

The runnable design-doc-review exemplar ships alongside this document in
`system-defaults/workflows/review/`, `system-defaults/brofiles/review/`, and
`system-defaults/agentic-corpus/packets/design-doc-review/`. Use it as the
starting point for tailoring.

## The Invariant Dimension Taxonomy

Every review ensemble — regardless of domain, language, or project — projects
five orthogonal review dimensions. Each dimension owns one question and defers
the rest. The dimensions are non-negotiable; the projections are the operator's
lever.

| # | Dimension | Code | The invariant question | Tie-break rule |
|---|-----------|------|----------------------|----------------|
| 1 | **Soundness** | `SND` | Is this real and correct — not noise, restatement, or a cheaper tool's job? | Rejects noise *because it isn't real*, not because it's cheap |
| 2 | **Precision** | `PRC` | Are claims precisely characterized with honest evidence grades? | Names the contract and grades the evidence; does not judge worth |
| 3 | **Economy** | `ECO` | Is the response proportional, bounded, and mapped to a real executor? | Rejects overkill *because a cheaper tool suffices*; does not judge correctness |
| 4 | **Resilience** | `RES` | What breaks if you act — hidden coupling, blast radius, migration risk? | Surfaces operator gates *as risk*; does not grade the underlying claim's evidence |
| 5 | **Corroboration** | `COR` | Does history/context confirm the signal, and is it deduped? | Owns history and deduplication; does not judge whether the fix is worth it |

**Why five, not more or fewer:** The pathology ensemble (see
`design/refactor-tools/pathology-ensemble-review.md`) demonstrated that five
orthogonal dimensions with clear tie-breaks is the right granularity for
multi-perspective review. Fewer leaves gaps in coverage; more creates overlap
and panel bloat without proportional signal gain.

## The Invariant Deliberation Protocol

The deliberation mechanism is bridgecrew-aligned and non-negotiable. It is
independent of the review domain:

- **Independent validator with exclusion teeth.** A dedicated validator runs
  *after* the blind lens round and *before* debate. It audits cited evidence,
  escalates only where weakness meets consequence, and posts one
  confirmed/refuted/inconclusive annotation per finding. Refuted findings are
  **excluded** from the review output — the engine derives each post's standing
  from its validation annotations, not from prompt enforcement.
- **Conflict-triggered debate.** Debate only fires when the validation round
  produces conflicts (posts where validators disagree with each other or where
  the evidence is ambiguous). When validation is clean, debate is skipped and
  the arc goes directly to resolve/synthesize.
- **Unresolved contradictions survive to the operator.** Genuine disagreement
  between lenses is valuable signal, not a failure. The review output carries
  an explicit "Contradictions Requiring Human Judgment" section. The system
  never force-resolves disagreement.
- **Evidence-backed, not prompt-enforced.** Validation verdicts are backed by
  `bbox_bundle_evidence` calls and reproducible operations, not by LLM
  assertions.

## Phase 1: Recon (Automated)

Survey the target repo. Run these checks before asking the operator anything:

```
1. Language(s), build system, framework
   - Primary language and version (Cargo.toml, package.json, pom.xml, etc.)
   - Build system (cargo, gradle, maven, npm, bazel, etc.)
   - Framework presence (Spring, Actix, Django, React, etc.)

2. Domain
   - What does this project do? (web service, CLI, library, embedded,
     data pipeline, compiler, game, etc.)
   - What are its primary entry points?

3. Existing conventions
   - CLAUDE.md / AGENTS.md / GEMINI.md (project-level instructions)
   - Linters, formatters, SAST tools configured
   - Test patterns (unit, integration, e2e, snapshot, property)
   - CI configuration

4. Available evidence authorities
   - bbox_project_list → is the project registered?
   - bbox_describe_schema → what entity types and edge families exist?
   - bbox_code_symbols → code navigation coverage
   - Language-specific tools: LSP (rust-analyzer, JDTLS), type systems,
     refactoring tools, code_nav depth

5. Provider landscape
   - bro_providers → what providers are configured?
   - Available models per provider
   - Any provider-specific constraints (e.g., gemini excluded)

6. Existing ensemble infrastructure
   - bbox_artifact_list → any existing workflows, brofiles, packets?
   - Any existing teams or teamplates?

7. Transcript history
   - bbox_search for recent review/design/architecture discussions
   - bbox_knowledge for settled decisions and conventions
```

Report the recon summary to the operator before proceeding to the interview.

## Phase 2: Interview (Conversational)

For each dimension, identify what recon **cannot** find and ask the operator.
Use `AskUserQuestion` or plain conversation. Key interview questions:

### Per-Dimension Questions

**Soundness (SND):**
- "What counts as a 'real' finding in this domain vs. noise? For example, in
  security review, a real vulnerability vs. a scanner false positive. In
  architecture review, a real ownership defect vs. a metric restatement."

**Precision (PRC):**
- "What evidence authorities does this project trust? For example, the compiler
  and type system, the test suite, formal verification, manual review sign-off,
  or specific linters/analyzers."
- "Are there authority grades the Precision lens should distinguish? (e.g.,
  `syntax_only` / `indexed_hints` / `lsp_verified` / `compiler-confirmed` for
  Rust; `verified_by_test` / `verified_by_lint` / `manual_inspection` for
  other languages.)"

**Economy (ECO):**
- "What's the right remediation granularity for this project? Single-PR fixes?
  Multi-phase migrations? What executor patterns make sense — shipped atoms,
  PD-manual, scripts, or something else?"
- "Are there existing static tools (linters, SAST, formatters) that already
  cover ground the Economy lens should defer to?"

**Resilience (RES):**
- "What are the critical blast-radius boundaries in this project? Public API
  surface, data migration paths, deployment seams, shared state?"
- "Are there operator-authority flags the Resilience lens should surface?
  (e.g., opt-outs for public API changes, repr changes, breaking migrations)"
- "What's the deployment/rollback model? Does the design affect running systems?"

**Corroboration (COR):**
- "Where does your project history live? GitHub issues/PRs, Forgejo, Jira,
  Linear, mailing lists, Slack channels, bbox transcripts?"
- "Are there specific historical signals the Corroboration lens should weigh
  heavily? (e.g., reverted commits on related paths, abandoned design attempts,
  explicit operator decisions to defer or reject similar approaches)"

### Workflow Integration Questions

- "What should happen when the review completes?" Options:
  - Write a review document to a file (default)
  - Post to a Slack channel
  - Trigger a follow-up workflow (e.g., PD-dispatch for implementation)
  - Callback to CI (gate a merge or deploy)
  - Something else?

- "Do you want a pre-review grounding step?" Options:
  - Run diagnostic atoms before the review (pathology-style)
  - Run SAST/linters and feed results into the review
  - Run a code survey for context
  - No pre-review step (lenses review the target directly — the exemplar default)

- "Do you want the review to gate something?" Options:
  - Block a PR until the review passes
  - Gate a deploy or release
  - Advisory only (no automated gating)

## Phase 3: Propose (Present Projected Lenses)

For each of the 5 dimensions, present the projected lens to the operator:

```
## Proposed Lens: Soundness (SND)
  - Lens ID: <project-prefix>-SND
  - Concrete question: "<domain-specific phrasing>"
  - Evidence authorities: <list of tools/operations>
  - Reject criteria: "<what this lens defers>"
  - Provider/model: <provider> / <model>
  - Integration points: <any special data sources>
```

Present all 5 projections together. Ask:

> "Review these 5 projected lenses. Approve all, or specify adjustments to
> individual projections. I'll incorporate your feedback before generating the
> artifacts."

Key projection guidance:

- **The question must be domain-specific** but must preserve the invariant
  concern. "Is this a real architectural defect?" (pathology) and "Does this
  design correctly solve the stated problem?" (design-review) are both
  Soundness projections — same invariant question, different domain vocabulary.
- **Evidence authorities must match the target repo.** A Rust project trusts
  rust-analyzer, clippy, and compiler diagnostics. A Python project trusts
  mypy, pytest, and type stubs. A design-doc review trusts the existing
  codebase as ground truth.
- **Reject criteria must be explicit.** Each lens must state what it *defers*
  to other lenses or to cheaper tools. This is what keeps lenses orthogonal.
- **Provider assignment should be heterogeneous.** Avoid putting the same
  provider+model on more than 2 lenses. Proven assignments from the pathology
  ensemble: Soundness → Claude (deepest judgment), Precision → Brodex/GPT
  (contract rigor), Economy → DeepSeek (cost reasoning), Resilience → GLM
  (adversarial risk), Corroboration → Brodex/GPT (history synthesis).

## Phase 4: Compose (Define the Workflow Shape)

Present the full workflow node graph. The baseline shape (from the exemplar):

```
Setup → BlindPost → ValidateBlind → TransitionToValidate →
Validate → ValidateValidation → [branch] →
  ready_debate → TransitionToDebate → Debate → ValidateDebate →
  ready_skip → TransitionToResolve
TransitionToResolve → SynthesizeReview → WriteReview → terminal
```

Customizations to propose based on the interview:

- **Pre-review steps:** Add a `Survey` node (atom dispatch, SAST run, or code
  grounding) before `BlindPost` if the operator wants pre-review enrichment.
- **Post-review steps:** Add follow-up nodes after `WriteReview` — trigger a
  workflow, post to a channel, run CI, gate a merge.
- **Callback integration:** Wire `on_arc_exit` hooks for post-arc actions.
- **Gate packet adjustments:** Modify participation rules if the panel size
  changes (the exemplar hardcodes `post_count ≥ 5` and all 5 aliases).

Ask:

> "Does this workflow shape match your needs? I can add pre-review grounding
> steps, post-review callbacks, or adjust the gate rules."

## Phase 5: Mint (Generate and Install Artifacts)

Generate the full artifact set:

1. **Workflow JSON** — schema-valid against `schema/workflow.schema.json`.
   Write to `system-defaults/workflows/review/<name>.json` (or the project's
   `.bbox/` if project-scoped).

2. **5 lens brofiles** — read-only, provider-pinned, each with:
   - `name`: `<name>-<dim-code>` (e.g., `security-review-snd`)
   - `provider` / `model` / `effort`: as proposed
   - `lens`: full prompt owning exactly one question
   - `filters.allow`: read-only tools + whiteboard post/annotate/vote
   - `filters.disallow`: all mutation tools, `bro_*`, `whiteboard_open/register/transition`

3. **Validator brofile** — read-only, evidence-prover:
   - `filters.allow`: read-only tools + `whiteboard_annotate` (no `whiteboard_post`)
   - `lens`: audit-first, escalate-only-where-weakness-meets-consequence, post-per-finding

4. **Facilitator brofile** — orchestrator:
   - `filters.allow`: read-only + `whiteboard_state/summarize/transition`
   - `lens`: distribute context, synthesize review, write output

5. **Panel teamplate** — 5 members:
   ```json
   {
     "name": "<name>-panel",
     "version": 1,
     "members": [
       { "brofile": "<name>-snd", "alias": "soundness", "count": 1 },
       { "brofile": "<name>-prc", "alias": "precision", "count": 1 },
       { "brofile": "<name>-eco", "alias": "economy", "count": 1 },
       { "brofile": "<name>-res", "alias": "resilience", "count": 1 },
       { "brofile": "<name>-cor", "alias": "corroboration", "count": 1 }
     ]
   }
   ```

6. **Gate packet** — participation rules:
   - `blind_all_lenses_posted`: phase=blind, post_count≥5, all 5 aliases `has_posted`
   - `validation_complete_with_conflicts`: phase=validate, unvalidated=0, conflict≥1
   - `validation_complete_no_conflicts`: phase=validate, unvalidated=0, conflict=0
   - `debate_participation_complete`: phase=debate, unreviewed=0, vote≥1
   - `invalid_default`: catch-all

### Install Sequence

```text
# Install each brofile
bbox_artifact_install(kind="brofile", source="<path-to-brofile>")

# Install the gate packet
bbox_artifact_install(kind="packet", source="<path-to-packet>")

# Install the panel teamplate
bbox_artifact_install(kind="team", source="<path-to-teamplate>")

# Install the workflow
bbox_artifact_install(kind="workflow", source="<path-to-workflow>")

# CRITICAL: Instantiate the team (save_template is NOT enough)
bro_team(action="create", name="<name>-panel", template="<name>-panel")

# Validate with dry-run
bro_orchestrate_run(workflow=<workflow-json>, dry_run=true)
```

The team instantiation step is required because the ensemble actor resolves
teams from the instantiated teams store, not from teamplates. This is the same
lesson learned from the pathology ensemble deployment.

## Signposts

The following system memories and design documents provide deeper context on the
mechanics this generator uses. Fetch them via `bbox_knowledge` when needed:

- **Workflow primitives:** `bbox_knowledge(query="sm-workflow-orchestration")`
  — actor kinds, transition types, hooks, vars_schema, subworkflows, wait nodes.
- **Whiteboard API:** `bbox_knowledge(query="sm-whiteboards")`
  — phases, posts, annotations, votes, transitions, conflict detection.
- **Design packets:** `bbox_knowledge(query="sm-rule-packets")`
  — how to encode evaluation criteria as rule-packets.
- **Pathology ensemble (reference):** `design/refactor-tools/pathology-ensemble-review.md`
  — the original projection of these 5 dimensions into architecture pathology.
- **Pathology dispatch (operator guide):** `docs/pathology-dispatch.md`
  — how pathology ensembles are invoked, monitored, and handed off.

## Worked Example: The Design-Doc Review Ensemble

A runnable exemplar ships alongside this generator:

| Artifact | Path |
|----------|------|
| Workflow | `system-defaults/workflows/review/design-doc-review.json` |
| Soundness lens | `system-defaults/brofiles/review/design-doc-review-snd.json` |
| Precision lens | `system-defaults/brofiles/review/design-doc-review-prc.json` |
| Economy lens | `system-defaults/brofiles/review/design-doc-review-eco.json` |
| Resilience lens | `system-defaults/brofiles/review/design-doc-review-res.json` |
| Corroboration lens | `system-defaults/brofiles/review/design-doc-review-cor.json` |
| Validator | `system-defaults/brofiles/review/design-doc-review-validator.json` |
| Facilitator | `system-defaults/brofiles/review/design-doc-review-facilitator.json` |
| Panel teamplate | `system-defaults/refactor/pathology/teamplates/design-doc-review-panel.json` |
| Gate packet | `system-defaults/agentic-corpus/packets/design-doc-review/whiteboard-participation.json` |

The design-doc-review projections:

| Dimension | Provider | The design-doc question |
|-----------|----------|----------------------|
| SND | `claude` / `claude-opus-4-8` | Does this design correctly solve the stated problem — no logical gaps, wrong assumptions, or solution-problem mismatch? |
| PRC | `brodex` / `gpt-5.5` | Are claims precisely specified (interfaces, contracts, data models, invariants) with honest scope boundaries? |
| ECO | `deepseek` / `deepseek-v4-pro` | Is scope bounded, decomposable, and proportional — not overengineered or duplicating existing capability? |
| RES | `glm` / `glm-5.1` | What breaks during/after implementation — migration surface, compat, hidden coupling, operational risk? |
| COR | `brodex` / `gpt-5.5` | Does this align with operator intent, prior decisions, and known constraints? |

This exemplar reviews a design document (provided as `design_doc_path` +
`design_doc_text` input vars) against the codebase. It has no pre-review atom
dispatch — the lenses ground directly against the code. Its output is a review
document written to `<project>/design/review/<slug>-review.md`.
