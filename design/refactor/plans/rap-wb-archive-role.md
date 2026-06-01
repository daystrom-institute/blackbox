---
title: "RAP-WB-ARCHIVE-ROLE: archive must share an explicit lifecycle/role authority contract"
kind: correction-plan
lifecycle: proposed
corpus: project-refactor
topic:
  - refactor-plan
  - architecture
date: 2026-06-01
baseline_commit: 551b1463d9d2603ba940b5d0f65f6b5fd3bb1b24
generated_by: rust-arch-pathology
scope: "src/whiteboards.rs"
brief: "Scope src/whiteboards.rs at baseline 551b1463d9d2603ba940b5d0f65f6b5fd3bb1. The resolved board leaves one actionable, gated architecture diagnosis: archive advances a board from Resolve to Archived outside the same role/transition boundary used by WhiteboardRegistry::transition. Broad WhiteboardRegistry registry/persistence/archive fusion remains inconclusive and is not an implementation slice."
---

# RAP-WB-ARCHIVE-ROLE: archive must share an explicit lifecycle/role authority contract

## Diagnosis Summary

- `{"id":"RAP-WB-ARCHIVE-ROLE","severity":"medium","status":"accepted_gated","summary":"Archive is a lifecycle/role-boundary concern, not a rustc/clippy/lint finding. WhiteboardRegistry::transition enforces registered-agent, Role::can_transition, and legal phase transition before mutating phase/history/persistence; WhiteboardRegistry::archive is called directly by the public whiteboard_archive tool and performs Resolve -> Archived plus archive persistence/removal with a weaker authority path."}`
- `{"id":"RUST-ARCH-WB-REGISTRY-FUSION","severity":"low","status":"inconclusive_deferred","summary":"WhiteboardRegistry breadth and method clustering were confirmed at indexed_hints grade, but panel challenges did not accept generic registry/persistence/god-impl breadth as standalone pathology. Do not remediate AP-WB-001/persistence extraction from this board without a separate bounded diagnosis."}`

## Evidence

- `{"claim":"Phase::canonical_next includes Resolve -> Archived.","ref":"src/whiteboards.rs:72-81"}`
- `{"claim":"WhiteboardRegistry::transition checks registered agent, Role::can_transition, legal phase transition, then updates phase/history/persistence.","ref":"src/whiteboards.rs:898-939"}`
- `{"claim":"WhiteboardRegistry::archive checks registered agent and Phase::Resolve, then sets Archived, appends phase_history, writes archive JSON, and removes active board state.","ref":"src/whiteboards.rs:941-1028"}`
- `{"claim":"The public whiteboard_archive tool calls archive directly rather than routing through transition authority.","ref":"src/tools/whiteboards.rs:468-477"}`
- `{"claim":"Validator confirmed archive-role code/provenance, clean parse/26 impl methods, and public-tool/gate facts; no validator refutations were recorded.","ref":"validator ann-003/ann-005/ann-006"}`
- `{"claim":"Authority is indexed_hints with advisory_severity=info and no proven crate-root re-export/signature delta; behavioral public-surface change remains gated.","ref":"rust_public_api_guard via post-004/ann-004"}`
- `{"claim":"No #[repr(...)] in src/whiteboards.rs, no active Cargo [features] section in inspected workspace, only #[cfg(test)] in scope, and archive-role evidence is not macro-expansion-dependent.","ref":"post-006 resilience survey"}`

## Atom Mapping

- `{"mapping":"atom:rust-architecture-impl-role-coherence@v1","notes":"Shipped diagnostic atom; accepted only for archive/transition role-boundary locus.","slice":"RAP-WB-ARCHIVE-ROLE-DIAGNOSIS"}`
- `{"mapping":"PD-manual","notes":"Human/operator must choose whether archive is a terminal transition governed by Role::can_transition or a separately documented privileged action.","slice":"RAP-WB-001 archive authority contract decision"}`
- `{"mapping":"PD-manual","notes":"Do not use broad god-impl/persistence extraction. If code is changed, keep the edit bounded to archive/transition authority, signal semantics, phase_history, persistence/removal choreography, and public-tool behavior.","slice":"RAP-WB-002 archive implementation slice"}`
- `{"mapping":"PD-manual","notes":"Run public API/tool-surface review plus cargo validation; no acknowledge_repr gate is currently triggered.","slice":"RAP-WB-003 validation and public-surface review"}`
- `{"mapping":"PD-manual deferred","notes":"Not accepted by this board as pathology; requires separate evidence before execution.","slice":"AP-WB-001 persistence extraction"}`

## Remediation Plan

- `{"actions":["Specify whether archive should be allowed only by facilitator/operator transition authority or by a distinct documented terminal authority.","Specify whether archive should emit the same transition/signal semantics as whiteboard_transition.","Specify whether currently ignored archive filesystem errors remain warnings or become propagated errors."],"gates":["acknowledge_public_api_change must be explicit if specialist callers may be rejected or public tool behavior changes","acknowledge_repr not required for current src/whiteboards.rs evidence"],"id":"RAP-WB-001","mapping":"PD-manual","title":"Decide archive authority contract before editing"}`
- `{"actions":["Unify or explicitly separate the authority check for Resolve -> Archived with the transition contract.","Preserve or deliberately change phase_history, archive JSON write, active-board removal, persistence, and transition-signal behavior according to RAP-WB-001.","Keep borrow/lock/double-persist fallout as validation risk, not as assumed architecture proof."],"gates":["Do not include AP-WB-001 persistence extraction or broad WhiteboardRegistry split in this slice."],"id":"RAP-WB-002","mapping":"PD-manual","title":"Implement only the archive-role boundary slice"}`
- `{"actions":["Run rust_public_api_guard/public-tool-surface review for whiteboard_archive and WhiteboardRegistry behavior.","Run cargo check default and cargo check --all-targets.","Run targeted cargo test whiteboards or equivalent whiteboard lifecycle tests covering authorized archive, unauthorized archive, phase_history, archive file movement, and signal semantics."],"id":"RAP-WB-003","mapping":"PD-manual","title":"Validate behavior and public surface"}`

## Acceptance Criteria

- RAP-WB-AC-001: {"criterion":"Plan records an explicit operator decision for archive authority and transition-signal semantics before any source change.","id":"RAP-WB-AC-001"}
- RAP-WB-AC-002: {"criterion":"If behavior changes for public WhiteboardRegistry or whiteboard_archive callers, acknowledge_public_api_change=true is present as a gate; acknowledge_repr is absent unless later work touches a repr-bearing struct.","id":"RAP-WB-AC-002"}
- RAP-WB-AC-003: {"criterion":"Implementation changes only the archive/transition boundary slice and does not perform broad registry/persistence/god-impl extraction.","id":"RAP-WB-AC-003"}
- RAP-WB-AC-004: {"criterion":"Validation includes cargo check, cargo check --all-targets, and targeted whiteboard lifecycle tests; no compiler/test authority is claimed before those pass.","id":"RAP-WB-AC-004"}
- AP-WB-001: {"criterion":"Deferred: persistence extraction is accepted only if a later board supplies independent architecture evidence, exact method set, executor mapping, and cargo/test validation.","id":"AP-WB-001"}

## Contradictions Requiring Human Judgment

- **ann-007 economy challenge on post-001** — Broad registry+persistence+archive fusion is too large for PD; only archive/transition or explicitly named per-slice executors are bounded enough.
- **ann-012 corroboration challenge on post-001** — History supports archive-role-bypass, not a standalone registry+persistence+archive fusion diagnosis.
- **ann-013 resilience challenge on post-001** — Acting on broad fusion risks public WhiteboardRegistry/tool behavior and borrow/lock fallout; requires public-API gate and cargo validation if not narrowed.
- **ann-018 corroboration challenge on post-002** — AP-WB-002 maps to archive-role locus, but AP-WB-001/persistence extraction lacks independent accepted pathology evidence.
- **ann-020 precision challenge on post-001** — Contract class must be named as internal public facade/API behavior plus lifecycle/archive semantics; external API break is unproven above indexed_hints.
- **ann-025 soundness challenge on post-001** — WhiteboardRegistry owning boards plus storage_dir may be coherent repository/storage design; only archive writes phase/history outside transition is pathology.
- **ann-026 soundness challenge on post-002** — Economy constraints do not make broad fusion real architecture; AP-WB-001 should not proceed from method clustering alone.
- **ann-016 resilience challenge on post-004** — rust_public_api_guard showing no signature/re-export delta does not clear behavioral public-API risk for whiteboard_archive authority changes.
- **ann-029 soundness challenge on post-004** — The no-hard-reject framing is too permissive if it preserves broad registry-fusion; behavior discussion should attach only to archive-role.

## Deferred

- `{"id":"AP-WB-001","reason":"Not accepted as standalone architecture pathology; severity capped low/inconclusive.","title":"Persistence/backend extraction from WhiteboardRegistry"}`
- `{"id":"broad-registry-god-impl","reason":"Panel challenges treat method count/cluster breadth as insufficient without archive-specific contract pressure.","title":"Generic WhiteboardRegistry god-impl or count/density diagnosis"}`
- `{"id":"cfg-macro-inflation","reason":"Current board evidence found no applicable feature scatter, no macro_rules dependency, and only #[cfg(test)] in scope.","title":"Feature/cfg/macro pressure for this locus"}`

## Dispatch Payload

```json
{
  "initial_vars": {
    "acceptance_criteria": [
      {
        "criterion": "Plan records an explicit operator decision for archive authority and transition-signal semantics before any source change.",
        "id": "RAP-WB-AC-001"
      },
      {
        "criterion": "If behavior changes for public WhiteboardRegistry or whiteboard_archive callers, acknowledge_public_api_change=true is present as a gate; acknowledge_repr is absent unless later work touches a repr-bearing struct.",
        "id": "RAP-WB-AC-002"
      },
      {
        "criterion": "Implementation changes only the archive/transition boundary slice and does not perform broad registry/persistence/god-impl extraction.",
        "id": "RAP-WB-AC-003"
      },
      {
        "criterion": "Validation includes cargo check, cargo check --all-targets, and targeted whiteboard lifecycle tests; no compiler/test authority is claimed before those pass.",
        "id": "RAP-WB-AC-004"
      },
      {
        "criterion": "Deferred: persistence extraction is accepted only if a later board supplies independent architecture evidence, exact method set, executor mapping, and cargo/test validation.",
        "id": "AP-WB-001"
      }
    ],
    "epoch": 0,
    "max_epochs": 3,
    "phase_doc_path": "design/refactor/plans/rap-wb-archive-role.md",
    "phase_doc_text": "<full correction plan text>",
    "project_dir": "/Users/invidious/repos/transcript-search",
    "target_context_window": 10000
  },
  "project_dir": "/Users/invidious/repos/transcript-search",
  "workflow_id": "phase-decompose-main-edit"
}
```
