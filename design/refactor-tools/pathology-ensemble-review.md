---
title: "Pathology Ensemble Review"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - pathology
  - orchestration
tags:
  - refactor-tools
  - pathology
  - ensemble
  - whiteboard
  - architecture
date: 2026-05-30
updated: 2026-05-31
status: "partial — sound. Five-dimension lens model + bridgecrew-aligned deliberation (independent validator with exclusion teeth, conflict-triggered debate, surviving contradictions) implemented across all four flows (arch+perf × java+rust). Pending a live end-to-end smoke run."
brief: "Replace the single-actor pathologist in the pathology flows with a heterogeneous review ensemble: five orthogonal review dimensions projected per language into lens instances, plus an independent validator. Deliberation follows bridgecrew — the validator traces each claim and refutes false positives (excluded from the plan), debate is conflict-triggered, and unresolved disagreement survives into the plan as contradictions requiring human judgment, not a consensus gate."
---

# Pathology Ensemble Review

> **Status (2026-05-31): the deliberation core has been redesigned and
> implemented.** The earlier consensus-by-concession mechanism — the
> `debate_resolved` gate that forced `unresolved_challenges == 0` — has been
> **replaced** with the bridgecrew-aligned model described below: an independent
> validator with exclusion teeth, conflict-triggered debate, and unresolved
> disagreement that survives into the correction plan. This now spans all four
> flows (arch + perf × java + rust). The "Workflow shape", "Gate packet", and
> "Acceptance criteria" sections describe the implemented design; the prior
> broken text has been removed.

## Why this exists

The previously shipped pathology workflows described a multi-specialist
whiteboard review in their design docs, but the implemented artifact used a
**single durable actor** (`pathologist`) that opened a whiteboard, posted every
claim under one identity, and transitioned `blind → read → debate → resolve` by
itself. The "debate" was one model talking to itself; the whiteboard transitions
were ceremony. All diagnostic diversity came from the diagnosis atoms — but every
atom dispatched through the **same** `*-pathologist` brofile (`codex` /
`gpt-5.5`), so there was zero provider, persona, or perspective diversity, and no
adversarial cross-check of any atom's output.

This is the documented failure mode of single-agent self-reflection. Huang et
al. (2024) found self-reflections "tend to repeat earlier misconceptions and do
not introduce new reasoning paths." Smit et al. (ICLR 2025) found *homogeneous*
multi-agent debate fails to beat a single agent, while *heterogeneous* panels
(genuinely different specializations) substantially outperform. MAR (Chen et al.
2024) shows separating the critic from the actor prevents the self-confirming
loop. A pathology run is exactly the case where this matters: an expensive,
infrequent, multi-dimensional judgment whose wrong answer costs a wasted PD
remediation epoch.

The **lens model** (five dimensions × language) and the **bridgecrew-aligned
deliberation** (independent validator, conflict-triggered debate, surviving
contradictions) are the two durable contributions, described in full below.

## The core idea: dimensions, projected per language

A good architecture-pathology *diagnosis review* has five orthogonal concerns —
the bridgecrew universal dimensions, re-tuned from "review a whole-system spec"
to "judge an atom-produced diagnosis candidate":

| # | Dimension | Code | The question this lens owns |
|---|-----------|------|-----------------------------|
| 1 | **Soundness** | `SND` | Is this a *real* architectural ownership/boundary/seam defect, evidence-supported — not a metric, lint, or compiler fact restated as architecture? |
| 2 | **Precision** | `PRC` | Are the affected contracts/interfaces precisely characterized, and is the *authority* of each claim honest (what is verified vs inferred)? |
| 3 | **Economy** | `ECO` | Is the remediation bounded and correctly mapped to an executor (shipped atom / gap ID / PD-manual), and is expensive LLM pathology the justified tool here versus cheaper static authority? |
| 4 | **Resilience** | `RES` | Blast radius: what breaks if you act on this — hidden coupling, dropped acceptance criteria, migration/compile risk, operator-authority opt-outs? |
| 5 | **Corroboration** | `COR` | Does operator/agent/git history confirm the pressure is real and worth acting on, and is the diagnosis deduped/merged rather than a smell dump? |

These five are **orthogonal** and language-agnostic in the abstract. But Java and
Rust pathology are not the same craft: Java's authorities are SAST / ArchUnit /
framework contracts / clone detection; Rust's are rustc / clippy / rust-analyzer
/ `cargo metadata` / feature-matrix, plus authority grades and operator-authority
opt-outs (`acknowledge_repr` RX-S1, `acknowledge_public_api_change` RX-E1) under
RX-V1, and LSP fail-closed behavior under RX-V3.

So the correct construct is five dimensions projected into language-specific
instances — a `dimension × language` matrix. Each projection keeps the
dimension's question but binds it to that language's evidence authorities, reject
criteria, and remediation vocabulary. A run instantiates one language's column
(5 lenses).

## The ten lens projections

Stable IDs are `PRD-<DIM>-<LANG>` (Pathology Review Dimension).

### Dimension 1 — Soundness (`SND`)

> Is the candidate a real architectural defect, or a metric/lint/compiler fact
> dressed as architecture?

**`PRD-SND-J` — Java Role & Responsibility Soundness.** Real ownership defect —
class/role mismatch, responsibility scattered/centralized with no canonical
owner, behavior on the wrong side of a data/behavior split. Authorities:
`bbox_code_symbols`, `bbox_code_refs`, `java_class_dependency_analysis`, declared
role signals. Reject: mechanically decidable by ArchUnit/clone-detector/metric.

**`PRD-SND-R` — Rust Impl/Module Role Soundness.** Real impl/module role
mismatch, state-ownership collapse, or construction-boundary collapse — not
method count, fan-out, or cfg density. Authorities:
`rust_impl_partition_analysis` (`deep_analysis=true`), `extract_rust_impl_methods`,
symbols, refactor status, receiver/module-path/trait/attr role signals. Reject:
clippy/rustc already names it (`type_complexity`, `large_enum_variant`,
`module_inception` are inputs, not diagnoses).

### Dimension 2 — Precision (`PRC`)

> Are the affected contracts pinned down, and is the authority of each claim
> honest?

**`PRD-PRC-J` — Java Contract & Framework Precision.** Name the contract at risk
(public API, checked/unchecked exception contract, framework lifecycle/role
obligation, serialization/reflection) and state observed vs assumed. Reject: a
contract claim that is a compile/typecheck fact, not an architectural one.

**`PRD-PRC-R` — Rust Contract & Authority Precision.** Name the contract (public
API/re-exports, error type + `Result` signature, trait boundary + object safety,
lifecycle/`Send`+`Sync`) and attach an **authority grade** to every claim
(`syntax_only`/`indexed_hints`/`lsp_verified`/compiler-confirmed). The
authority-grade auditor. Authorities: `rust_public_api_guard`, object-safety
reports, rust-analyzer-backed kinds (RX-V3: fail closed on `lsp_unavailable`,
never silently downgrade). Reject: an indexed-hint overclaimed as
`lsp_verified`/compiler-confirmed.

### Dimension 3 — Economy (`ECO`)

> Is remediation bounded and mapped to a real executor, and is pathology the
> justified tool?

**`PRD-ECO-J` — Java Remediation Economy.** Bounded slice + named executor
(shipped Java atom or PD-manual); justify LLM pathology over ArchUnit/lint/clone
detector. Reject: cheapest correct fix is a static rule.

**`PRD-ECO-R` — Rust Remediation Economy & Atom Mapping.** Every slice carries a
shipped atom by exact manifest name, a G-series gap ID, or `PD-manual`
("future atom" without an ID forbidden); complete `## Atom Mapping`; cargo-only
validation per RX-V2. Reject: clippy/rustc-repairable, no architectural slice.

### Dimension 4 — Resilience (`RES`)

> What breaks if you act on this?

**`PRD-RES-J` — Java Change Blast Radius.** Hidden coupling surfaced by
extraction, dropped ACs, merge surface, framework lifecycle breakage,
reflection/serialization consumers. Reject: no architectural risk — mechanical,
locally validated.

**`PRD-RES-R` — Rust Change Blast Radius & Operator Gates.** Borrow/compile-fix
risk, feature/cfg-matrix breakage, macro-blindness, `?`-conversion gaps; surface
every operator-authority opt-out as a **gate** (`acknowledge_repr` RX-S1,
`acknowledge_public_api_change` RX-E1, never assumed — RX-V1). Reject:
borrow/conversion outcome that is the compiler's call (recorded as uncertainty).

### Dimension 5 — Corroboration (`COR`)

> Does history confirm the pressure, and is the diagnosis set deduped?

**`PRD-COR-J` — Java Transcript-Anchored Pressure.** Confirm/down-rank against
operator/agent history + churn/fix-revert; merge overlapping atom signals.
Authorities: `bbox_search`/`bbox_notes`/`bbox_thread_list`/`bbox_hybrid_search`,
git log, `bbox_blame`. Reject: history alone, unconfirmed by current code.

**`PRD-COR-R` — Rust Transcript-Anchored Pressure.** Same, with Rust signals:
public-API opt-out debates, compile-fix churn, abandoned module-split/bin-to-lib
plans (G22 / `note-c699da56` caveat), failed cargo loops. Reject: count/narrative
pressure with no architectural claim.

### The matrix at a glance

| Dimension ↓ / Language → | Java | Rust |
|---|---|---|
| Soundness (`SND`) | `PRD-SND-J` Role & Responsibility | `PRD-SND-R` Impl/Module Role |
| Precision (`PRC`) | `PRD-PRC-J` Contract & Framework | `PRD-PRC-R` Contract & Authority grade |
| Economy (`ECO`) | `PRD-ECO-J` Remediation Economy | `PRD-ECO-R` Economy & Atom Mapping |
| Resilience (`RES`) | `PRD-RES-J` Change Blast Radius | `PRD-RES-R` Blast Radius & Operator Gates |
| Corroboration (`COR`) | `PRD-COR-J` Transcript Pressure | `PRD-COR-R` Transcript Pressure (Rust signals) |

### Orthogonality tie-breaks

Each lens owns one question and defers the rest:

- **`SND` owns "is this architecture?"** — rejects lint-shaped findings *because
  they aren't architecture*, not because they're cheap.
- **`PRC` owns "is the claim precisely and honestly evidenced?"** — named
  contract + authority grade; does not judge whether the fix is worth it.
- **`ECO` owns "is remediation bounded and worth an LLM pathology pass?"** —
  rejects lint-shaped findings *because a static rule is the cheaper executor*.
- **`RES` owns "what breaks if you act?"** — surfaces operator gates *as risk*;
  `PRC` only grades the underlying claim's authority.
- **`COR` owns "history + dedupe."**

## Provider heterogeneity

The panel is drawn from four providers — `claude`, `deepseek`, `glm`, `brodex`
(`gemini` is deliberately excluded). `brodex` is the Codex/ChatGPT backend over
the OpenAI Responses transport via `bro-harness` — a different dispatch path from
the `codex`-CLI facilitator. Models pinned to current catalog ids
(`src/orchestration/providers/catalog.rs`):

| Dimension | Provider | Model | Rationale |
|---|---|---|---|
| Soundness | `claude` | `claude-opus-4-8` | deepest architectural-ownership judgment |
| Precision | `brodex` | `gpt-5.5` | Codex-family type/contract rigor; matches Rust authority-grade discipline |
| Economy | `deepseek` | `deepseek-v4-pro` | cost/tractability and executor-mapping reasoning |
| Resilience | `glm` | `glm-5.1` | adversarial blast-radius / risk framing |
| Corroboration | `brodex` | `gpt-5.5` | strong long-context history synthesis on a current model |

**Known homogeneity:** Precision and Corroboration are both `brodex` / `gpt-5.5`
— identical provider and model, differing only by lens prompt. This is the one
pair where the panel is *not* heterogeneous, an accepted trade against the
alternatives (`deepseek-reasoner` is three generations old; a fifth distinct
provider would mean `gemini`, which is excluded). If a fifth genuinely-distinct
current model becomes acceptable, Corroboration is the slot to move.

All five run at `effort: high` where the provider honors effort. All five are
read-only: the lens brofiles disallow `Write`/`Edit`/`Bash`/`bro_*`/
knowledge-mutation, and allow only the read/measure/whiteboard surface. Dispatch:
`claude` direct via the Claude Code CLI; `deepseek`/`glm`/`brodex` via
`bro-harness` (`deepseek`/`glm` Anthropic transport, `brodex` Responses).

The arch table above is the original arch panel. Two refinements are now in
effect:

- **No `claude` pins in the new artifacts.** The perf panels mirror the five
  dimensions but place **Soundness on `deepseek` / `deepseek-v4-pro`** (the
  strongest non-`brodex` reasoning model) rather than `claude`, keeping
  provider spread (deepseek ×2, brodex ×2, glm ×1) without a `claude` pin.
- **The independent validator is `brodex` / `gpt-5.5` at `effort: high`** — the
  Codex-family type/contract-tracing rigor that matches "read the library source
  yourself." It is a separate read-only actor, not a panel member; sharing the
  GPT family with Precision/Corroboration is harmless because it runs in its own
  phase and never participates in debate.

The **facilitator** reuses the per-language `*-architecture-pathologist` /
`*-performance-pathologist` brofiles (codex / gpt-5.5 — the established
orchestrator pin), running the survey and synthesis under `agent_name=facilitator`.
Facilitator and panel are tuned independently; the facilitator sharing
`codex`/GPT family with the Precision/Corroboration lenses is harmless because the
facilitator is not a debate participant.

## The independent validator (exclusion teeth)

The load-bearing addition over the old single-actor design is a dedicated
**validator** that runs *after* the blind lens round and *before* debate, in the
whiteboard's `validate` phase. It is one read-only actor per flow
(`{arch,perf}-{java,rust}-pathology-validator`). Its job, ported from bridgecrew
`R10.5`: for each lens post, isolate the single falsifiable claim, **trace it to
ground truth itself** (it does not trust the lens's inference about the
compiler / framework / library — it reads the actual source), and post exactly
one `whiteboard_annotate(type=validation, result=confirmed|refuted|inconclusive)`.

The exclusion is **engine-computed**, not prompt-enforced. `Board::post_standing`
(`src/whiteboards.rs`) derives each post's standing from its validation
annotations — precedence: any `refuted` → **Excluded**; else any `inconclusive`
(no confirmed) → **Inconclusive** (survives, severity-capped); else ≥1 `confirmed`
→ **Confirmed**; else **Unvalidated** (survives with a warning). `whiteboard_summarize`
surfaces `surviving_post_ids` / `excluded_post_ids` plus per-standing counts, so a
refuted finding **cannot** reach the correction plan: the synthesis prompt builds
only from `surviving_post_ids`, and the renderer lists excluded posts in a
`## Refuted Findings` appendix.

## Workflow shape (one skeleton, all four flows)

The node graph mirrors `phase-decompose-ensemble-decompose` but inserts the
validate phase and replaces the consensus gate:

```text
Setup            "" actor: default vars, baseline commit (git rev-parse HEAD),
                 whiteboard_open (opened_by=facilitator), register the 5 lens
                 aliases (soundness/precision/economy/resilience/corroboration)
                 + the validator alias (validator)
Survey           facilitator: cheap measurement, select bounded atom_requests
                 + normalize_*_pathology_atom_requests (allowlist-enforced)
FocusedAtoms     foreach atom_request (parallelism 3) → atom_results
                 (child subworkflow: atom_invoke → bro_wait → atom_status)
BlindPost        panel (kind: ensemble): each lens posts ONE blind entry under
                 its alias, setting target_file/location/finding_refs/severity
ValidateBlind    gate → ready → TransitionToValidate ; invalid → BoardInvalid
TransitionToValidate  facilitator: blind → read → validate
Validate         validator: trace each claim, post one validation annotation per
                 post (confirmed/refuted/inconclusive)
ValidateValidation  gate → ready_debate (conflicts) → TransitionToDebate ;
                 ready_skip (no conflicts) → TransitionToResolve ; invalid → BoardInvalid
TransitionToDebate  facilitator: validate → debate
Debate           panel: conflict-triggered, targeted challenge/corroborate + vote;
                 every surviving post gets cross-agent review; genuine disagreement
                 is LEFT as an unresolved challenge (no forced resolution)
ValidateDebate   gate (participation/coverage, NOT unresolved=0) →
                 ready → TransitionToResolve ; invalid → Debate (loop)
TransitionToResolve  facilitator: {validate|debate} → resolve + whiteboard_summarize
                 → board_summary (surviving/excluded/contradiction signals)
Synthesize       facilitator: build plan from surviving posts only, exclude refuted,
                 carry unresolved challenges into `contradictions`, refuted into
                 `refuted_findings`; on_exit parse_json → plan_json
WritePlan        write_*_pathology_plan hook → design/refactor[/perf]/plans/<slug>.md
```

The conflict-free `validate → resolve` skip (a new allowed transition in
`Phase::allows_transition_to`) realizes bridgecrew's "skip debate when zero
conflicts." `ValidateDebate` loops back to `Debate` on insufficient participation
(a hard max-rounds backstop bounds it), never on outstanding challenges —
unresolved challenges are the point, not a failure.

### Why the panel reviews atoms rather than replacing them

The atoms stay (cheap, bounded, single-question producers, per-atom SAST/compiler
gate). The ensemble is adversarial review of the atom outputs across the five
orthogonal axes, and the validator is the independent critic that gives refutation
teeth.

## Gate packet (`pathology-review/whiteboard-participation`, v2)

Lattice `["ready_debate","ready_skip","ready","invalid"]`; first-match evaluation;
reads `vars.board_check.*` (the `whiteboard_summarize` output). Four rules plus the
catch-all:

- `blind_all_lenses_posted` → `ready` when phase=blind, post_count≥5, and all five
  lens aliases `has_posted` (participation).
- `validation_complete_with_conflicts` → `ready_debate` when phase=validate,
  post_count≥5, `unvalidated_count=0` (every post validated — the validator
  annotates rather than posts, so completeness is proven by zero unvalidated
  posts), and `conflict_count≥1`.
- `validation_complete_no_conflicts` → `ready_skip` when phase=validate,
  post_count≥5, `unvalidated_count=0`, and `conflict_count=0`.
- `debate_participation_complete` → `ready` when phase=debate, post_count≥5,
  **`unreviewed_post_count=0`** (every post got a cross-agent challenge or
  corroborate — closes the "zero challenges trivially ready" hole) and
  `vote_count≥1`. It **does not read `unresolved_challenges`** — unresolved
  challenges survive into the plan.
- `invalid_default` → `invalid`.

## Plan-document shape

The emitted correction plan keeps the existing frontmatter and sections
(`Diagnosis Summary`, `Evidence`, `Authority Grades` + `Atom Mapping`,
`Remediation Plan`, `Acceptance Criteria` with `AP-*`/`RAP-*`/`PP-*` ids,
`Deferred`, `Dispatch Payload`) and adds two: a `## Contradictions Requiring
Human Judgment` section (surviving unresolved challenges, never force-resolved)
and a `## Refuted Findings` appendix (validator-excluded posts with their trace,
omitted when nothing was refuted). The PD-dispatch handoff
(`phase-decompose-main-edit`) is unaffected.

## Artifacts (installed)

| Kind | Artifact |
|---|---|
| lens brofile ×20 | `system-defaults/brofiles/refactor/pathology-lens/{java,rust}-pathology-{soundness,precision,economy,resilience,corroboration}.json` (arch) and `{java,rust}-pathology-perf-{…}.json` (perf) |
| validator brofile ×4 | `…/pathology-lens/{arch,perf}-{java,rust}-pathology-validator.json` (brodex / gpt-5.5, read-only) |
| panel teamplate ×4 | `system-defaults/refactor/pathology/teamplates/{java,rust}-pathology-panel.json` (arch) + `{java,rust}-perf-pathology-panel.json` (perf) |
| facilitator brofile ×4 | `{java,rust}-architecture-pathologist.json` (v2, `agent_name=facilitator`) + `{java,rust}-performance-pathologist.json` |
| packet ×1 | `…/packets/pathology-review/whiteboard-participation.json` (v2; shared by all four flows) |
| workflow ×4 | `system-defaults/workflows/refactor/{arch-pathology-java,arch-pathology-rust,perf-pathology-java,perf-pathology-rust}.json` (v2) |

The language-agnostic `perf-pathology.json` (v1) is superseded by the two
per-language perf workflows; `docs/perf-pathology-dispatch.md` should be updated
to point at them.

**Installation note (verified during the live smoke).** The `kind: ensemble`
actor resolves its `team` via `load_team` (`src/orchestration/team.rs`), which
reads an **instantiated team** from the teams store — NOT a teamplate, and with
no teamplate fallback. So installing the panel as a `team` *artifact* or saving
it as a *teamplate* is not enough; `bro_orchestrate_run` fails the BlindPost
dispatch with `Unknown team: <panel>` until the team is instantiated. Required
per-flow adoption sequence:

1. Install the five lens brofiles + the validator brofile + the facilitator.
2. Save the panel teamplate (`bro_team save_template`, or install the team
   artifact).
3. **Instantiate the team:** `bro_team(action="create", name=<panel>,
   template=<panel>)` — this writes the team into the teams store that
   `load_team` reads. Without this step the ensemble cannot dispatch.

Neither a `save_template` nor a daemon restart substitutes for step 3.

## Acceptance criteria

`PE-2` (the consensus-gate criterion) is **superseded** by `PE-2′` below.

- `PE-1`: Each flow dispatches a 5-member lens ensemble (no `claude` pin in the
  perf panels — Soundness is `deepseek`) plus a separate `codex` facilitator and
  a separate `brodex`/`gpt-5.5` validator.
- `PE-2′` (supersedes `PE-2`): `BlindPost` cannot advance until all five lenses
  post; the validator round must complete (`unvalidated_count=0`) before debate;
  debate gates on participation/coverage (`unreviewed_post_count=0`, a vote cast)
  and **never** on `unresolved_challenges`. Unresolved challenges survive into the
  plan's `Contradictions` section; validator-refuted findings are excluded from
  remediation and recorded in the `Refuted Findings` appendix.
- `PE-3`: Every panel member and the validator are read-only.
- `PE-4`: Rust `RES`/`PRC` lenses surface `acknowledge_repr` /
  `acknowledge_public_api_change` and authority grades as gates per RX-V1/RX-V3.
- `PE-5`: Each `normalize` op passes an explicit `allowed_atoms`.
- `PE-6`: A no-edit smoke run reaches `WritePlan` for each of the four flows.

## Open questions

1. **Atom-time diversity.** This design diversifies the *review*, not atom
   execution — all atoms still run through the facilitator brofile.
2. **Five vs adaptive panel size.** Fixed-5 implemented; revisit for scoped runs.
3. **One validator vs per-cluster validators.** bridgecrew runs one validator per
   target-file cluster; this implements a single validator per flow. Fan-out is a
   future enhancement if validation latency or coverage becomes a bottleneck.

## Relationship to existing designs

- Implements the review phase promised by
  [Architecture Pathology](arch-pathology.md) and
  [Rust Architecture Pathology](rust/rust-arch-pathology.md).
- The lens model mirrors the ensemble shape of
  [Phase-Decomposer Dispatch](../../docs/pd-dispatch.md) /
  `phase-decompose-ensemble-decompose`.
- The deliberation **follows** the bridgecrew adversarial-review plugin
  (`daystrom-institute/claude-plugins`, `bridgecrew/REVIEW_BOOTSTRAP.md` R10.5
  Validator + R12.1 sweep): independent refutation with exclusion teeth,
  conflict-triggered targeted debate, and disagreement surviving to the operator.
  The whiteboard primitives were reverse-engineered against the phaser whiteboard
  server (`daystrom-institute/claude-plugins`, `phaser/servers/whiteboard.js`),
  which validated the blind-visibility and self-annotation guards reused here.
