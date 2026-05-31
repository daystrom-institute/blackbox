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
status: "PARTIAL/UNSOUND — the lens projections (this doc's core) are reviewed and sound; the DELIBERATION/DEBATE core is broken and must be redesigned before implementation. See the disclaimer below."
brief: "Replace the single-actor pathologist in arch-pathology-{java,rust} with a heterogeneous review ensemble: five orthogonal review dimensions projected per language into ten lens instances. The lens model is sound; the whiteboard deliberation/debate mechanism specified here is NOT — it is consensus-by-concession, not adversarial review, and is flagged for redesign."
---

# Pathology Ensemble Review

> ## ⚠️ HANDOFF DISCLAIMER — THE DEBATE / DELIBERATION CORE IS WRONG
>
> **What is sound and reviewed:** the five-dimension stratification, the ten
> `dimension × language` lens projections, the provider heterogeneity model, and
> the lens brofiles/teamplates. These were operator-steered and are worth keeping.
>
> **What is broken (do NOT implement as specified):** everything describing the
> whiteboard *deliberation* — the "## Workflow shape" `BlindReview → Debate →
> ResolveDebate → ValidateDebate` flow, the `pathology-review/whiteboard-participation`
> packet's `debate_resolved` rule, and any claim that this constitutes a "debate."
> It does not. The specific defects, for whoever redesigns this:
>
> 1. **The gate forces consensus, which is anti-debate.** `ValidateDebate`
>    advances only when `unresolved_challenges == 0`. Zero challenges satisfies it
>    instantly, so the structure *rewards not challenging* — every challenge is
>    pure cost with no offsetting reward. A silent panel passes the gate.
> 2. **There is no independent refutation with teeth.** Challenges are cleared
>    only by the *owner of the challenged post* (whiteboards.rs enforces: you
>    cannot resolve your own challenge, and a post owner self-resolves). The party
>    under scrutiny decides whether the scrutiny stands; the challenger gets no
>    rebuttal. Nothing traces a claim to ground truth and *excludes* it.
> 3. **Single pass, no escalation.** Challenge-creation runs once; only resolution
>    loops. No counter-rebuttal, no convergence-through-argument.
>
> **The target model is bridgecrew** (`daystrom-institute/claude-plugins`,
> `bridgecrew/REVIEW_BOOTSTRAP.md`): an independent **Validator** role (R10.5)
> that distrusts the adversary, traces each claim to source, and posts
> `confirmed`/`refuted`/`inconclusive` — *refuted findings are excluded*; a
> **conflict-triggered, targeted** debate (R12.1 Phase 2.5) where the facilitator
> routes specific conflicting parties to engage; and crucially, **unresolved
> disagreement survives into the output as "contradictions requiring human
> judgment"** rather than being forced to zero. The redesign should: (a) add an
> independent validation/refutation step with exclusion teeth, (b) replace the
> consensus gate with participation+validation gating only, letting unresolved
> challenges flow into the correction plan, (c) make debate conflict-triggered,
> and (d) move convergence judgment into the facilitator agent rather than a
> static gate predicate. See the bridgecrew research section in
> `bridgecrew/REVIEW_DESIGN.md` ("Why Multi-Agent Might Work Here") for the
> heterogeneity rationale this whole design rests on.
>
> The "Workflow shape", "Gate packet", and "Acceptance criteria" sections below
> are preserved as-authored for reference, but are the part that was never
> actually adversarial. Read them as a record of the wrong approach.

## Why this exists

The shipped `arch-pathology-java` and `arch-pathology-rust` workflows describe a
multi-specialist whiteboard review in their design docs, but the implemented
artifact uses a **single durable actor** (`pathologist`) that opens a
whiteboard, posts every claim under one identity, and transitions
`blind → read → debate → resolve` by itself. The "debate" is one model talking
to itself; the whiteboard transitions are ceremony. All diagnostic diversity
comes from the diagnosis atoms — but every atom dispatches through the **same**
`*-architecture-pathologist` brofile (`codex` / `gpt-5.5`), so there is zero
provider, persona, or perspective diversity, and no adversarial cross-check of
any atom's output.

This is the documented failure mode of single-agent self-reflection. Huang et
al. (2024) found self-reflections "tend to repeat earlier misconceptions and do
not introduce new reasoning paths." Smit et al. (ICLR 2025) found *homogeneous*
multi-agent debate fails to beat a single agent, while *heterogeneous* panels
(genuinely different specializations) substantially outperform. MAR (Chen et al.
2024) shows separating the critic from the actor prevents the self-confirming
loop. A pathology run is exactly the case where this matters: an expensive,
infrequent, multi-dimensional judgment whose wrong answer costs a wasted PD
remediation epoch.

The **lens model** below is the durable contribution. The deliberation wiring is
not (see the disclaimer).

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

The **facilitator** reuses the existing `java-architecture-pathologist` and
`rust-architecture-pathologist` brofiles (codex / gpt-5.5 — the established
orchestrator pin), running the survey and synthesis. Facilitator and panel are
tuned independently; the facilitator sharing `codex`/GPT family with the
Precision/Corroboration lenses is harmless because the facilitator is not a
debate participant.

---

> The remaining sections (Workflow shape, Gate packet, Acceptance criteria) are
> the BROKEN deliberation design, preserved for the handoff per the disclaimer at
> the top. They are not sound. Do not implement as written.

## Workflow shape (both languages, one skeleton) — ⚠️ UNSOUND, see disclaimer

The facilitator role is retained; the panel is the five-lens ensemble. The node
graph mirrored `phase-decompose-ensemble-decompose`:

```text
Setup            "" actor: default vars, baseline commit (git rev-parse HEAD),
                 whiteboard_open (opened_by=facilitator), register 5 panel aliases
                 (soundness/precision/economy/resilience/corroboration)
Survey           facilitator: cheap measurement, select bounded atom_requests
                 + normalize_*_pathology_atom_requests (allowlist-enforced)
FocusedAtoms     foreach atom_request (parallelism 3) → atom_results
                 (unchanged child subworkflow: atom_invoke → bro_wait → atom_status)
BlindReview      panel (kind: ensemble): each lens posts ONE blind whiteboard
                 entry under its own alias
ValidateBlind    gate domain=pathology-review/whiteboard-participation
                 → ready → TransitionToDebate ; invalid → BoardInvalid
TransitionToDebate  facilitator transitions: blind → read → debate
Debate           panel: annotate, vote, raise challenges (single pass)
ResolveDebate    panel: post-owners resolve challenges against their posts
ValidateDebate   ⚠️ gate: unresolved_challenges=0 & vote_count≥1  ← THE BROKEN
                 CONSENSUS GATE. ready → TransitionToResolve ; invalid → ResolveDebate
TransitionToResolve facilitator transition debate → resolve + whiteboard_summarize
Synthesize       facilitator: merge debate-survived diagnoses → correction-plan JSON
                 (authority_grades + atom_mapping for Rust; AP/RAP criteria);
                 on_exit parse_json → plan_json
WritePlan        write_*_pathology_plan hook → design/refactor/plans/<slug>.md
```

A bounded `MoreEvidence` re-survey loop was considered and dropped: a correct
loop needs per-round whiteboards (phases are forward-only) plus cross-round
`atom_results` accumulation.

### Why the panel reviews atoms rather than replacing them

The atoms stay (cheap, bounded, single-question producers, per-atom SAST/compiler
gate). The ensemble was meant to be adversarial review of the atom outputs across
the five orthogonal axes — but as wired it is not adversarial (see disclaimer).

## Gate packet — ⚠️ the `debate_resolved` rule is the broken gate

`pathology-review/whiteboard-participation` (modeled on
`phase-decompose/whiteboard-participation`), lattice `["invalid","ready"]`. Reads
`vars.board_check.*` (the `whiteboard_summarize` output stored by the workflow).

- `blind_all_lenses_posted` → `ready` when phase=blind, post_count≥5, and all
  five aliases `has_posted` — **this rule is fine** (participation check).
- `debate_resolved` → `ready` when phase=debate, post_count≥5,
  `unresolved_challenges=0`, vote_count≥1 — **this is the anti-debate consensus
  gate. Replace it.** Forcing `unresolved_challenges=0` is what rewards silence.
- `invalid_default` → `invalid`.

## "Up-to-date with respect to latest code"

The current pathology workflows are `version 1` and predate the engine features a
real implementation would rely on, all of which exist in the current daemon
(verified against `phase-decompose-ensemble-decompose` `version 20`): `kind:
ensemble` actors bound to a `team`; `gate` nodes with `branch` `next`;
`parse_json` with `repair_missing_closing_delimiters`; the full
`whiteboard_*` surface. The custom hook ops
(`normalize_*_pathology_atom_requests`, `write_*_pathology_plan`) are registered.
The Java `normalize` op already falls back to a hardcoded Java default allowlist
(`ARCH_DEFAULT_ALLOWED_ATOMS`); passing `allowed_atoms` explicitly is robustness,
not a security fix.

## Artifacts (restored to disk; NOT installed)

| Kind | Artifact | Status |
|---|---|---|
| brofile ×10 | `system-defaults/brofiles/refactor/pathology-lens/{java,rust}-pathology-{soundness,precision,economy,resilience,corroboration}.json` | **sound, restored** |
| team ×2 | `system-defaults/refactor/pathology/teamplates/{java,rust}-pathology-panel.json` | **sound, restored** |
| packet ×1 | `system-defaults/agentic-corpus/packets/pathology-review/whiteboard-participation.json` | restored; `debate_resolved` rule unsound |
| brofile ×2 | reuse `java-architecture-pathologist`, `rust-architecture-pathologist` as facilitator | needs `agent_name=facilitator` fix on adoption |
| workflow ×2 | `arch-pathology-{java,rust}.json` | **NOT restored — left at v1; the v2 deliberation core was broken** |

## Plan-document shape (unchanged)

The emitted correction plan keeps the existing frontmatter and sections
(`Diagnosis Summary`, `Evidence`, `Authority Grades` + `Atom Mapping` for Rust,
`Remediation Plan`, `Acceptance Criteria` with `AP-*`/`RAP-*` ids, `Deferred`,
`Dispatch Payload`). The PD-dispatch handoff (`phase-decompose-main-edit`) is
unaffected.

## Acceptance criteria — ⚠️ PE-2 encodes the broken gate; supersede on redesign

- `PE-1`: Each language workflow dispatches a 5-member ensemble drawn from
  `claude`/`deepseek`/`glm`/`brodex` (one provider reused on a distinct... see
  Known homogeneity; `gemini` excluded) plus a separate `codex` facilitator.
- `PE-2`: ⚠️ "BlindReview cannot advance until all five posted, and Debate cannot
  advance with any unresolved challenge" — the second half is the consensus-gate
  defect. A correct criterion: disagreement *survives* into the plan; gating is
  participation + validation only.
- `PE-3`: Every panel member is read-only.
- `PE-4`: Rust `RES`/`PRC` lenses surface `acknowledge_repr` /
  `acknowledge_public_api_change` and authority grades as gates per RX-V1/RX-V3.
- `PE-5`: Java `normalize` passes an explicit `allowed_atoms`.
- `PE-6`: A no-edit smoke run reaches `WritePlan`.

## Open questions

1. **Atom-time diversity.** This design diversifies the *review*, not atom
   execution — all atoms still run through the facilitator brofile.
2. **Five vs adaptive panel size.** Fixed-5 proposed; revisit for scoped runs.

## Relationship to existing designs

- Implements the review phase promised by
  [Architecture Pathology](arch-pathology.md) and
  [Rust Architecture Pathology](rust/rust-arch-pathology.md).
- The lens model mirrors the ensemble shape of
  [Phase-Decomposer Dispatch](../../docs/pd-dispatch.md) /
  `phase-decompose-ensemble-decompose`.
- The deliberation **should** follow the bridgecrew adversarial-review plugin
  (`daystrom-institute/claude-plugins`, `bridgecrew/REVIEW_BOOTSTRAP.md` R10.5
  Validator + R12.1 sweep) — independent refutation with exclusion teeth,
  conflict-triggered targeted debate, and disagreement surviving to the operator.
  The deliberation specified in this doc does NOT, and is flagged for redesign.
