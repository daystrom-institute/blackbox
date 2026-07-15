---
title: "Model-visible World State for bro-harness"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - context-management
brief: "Generalizes bro-harness's TurnContextItem and dispatch emitted baselines into ordered typed sections with stable IDs, serializable snapshots, absent/unknown/known restore semantics, section-owned diffs, and retained-fragment reconciliation. World State records what the model is expected to know across resume and compaction; it is not a generic application-state or event ledger."
---

# Model-visible World State for bro-harness

## 0. Decision

Replace the narrow `reference_context_item` plus ad hoc emitted-baseline cells
with one typed **Model World State**. A section records the comparison state
needed to decide what the model must be told next. It does not own the underlying
application state and is not a general persistence framework.

The design is informed by the
[Codex context finding](../../research/harness/codex/codex-context-management.md)
but starts from bro-harness's shipped context and `side` persistence spine.

## 1. Existing seeds

bro-harness already has three partial implementations of the same idea:

- `TurnContextItem` persists cwd, shell, date, and timezone as the environment
  comparison baseline.
- `DispatchState` persists last-emitted scope and pin renderings.
- the transport buffer retains actual contextual fragments through ordinary
  turns and compaction according to provider-specific history rules.

These cells answer "did this one value change?" They do not provide one ordered
catalog of model-visible state, typed migration, or a repair path when a
persisted baseline survives but its rendered fragment does not.

## 2. Scope boundary

World State contains only mutable state that affects what the model should know.
Initial candidates:

- environment context;
- AGENTS/project instructions and their loaded-path fingerprint;
- dispatch scope and pins;
- collaboration/agent capability mode;
- visible and deferred tool-manifest identity;
- selected skill or system-memory lenses;
- permission/sandbox profile when the harness owns a faithful source.
- fleet connection and live-attempt availability plus blackops collaboration,
  operational policy, and corpus-capability availability as typed worker inputs.

It does not contain:

- todos, goals, clipboard values, LSP diagnostics, or code-mode KV values;
- release history, artifact inventory, or audit events;
- daemon stores, agent mailboxes, or scheduler state;
- static base instructions that never vary inside a session.

Those may have their own persistence and may contribute a rendered fragment,
but World State stores only the comparison snapshot for that fragment.

## 3. Section contract

The initial Rust shape should be small:

```rust
enum PreviousSection<'a, T> {
    Absent,
    Unknown,
    Known(&'a T),
}

trait WorldStateSection: Send + Sync + 'static {
    const ID: &'static str;
    type Snapshot: Serialize + DeserializeOwned;

    fn snapshot(&self) -> Self::Snapshot;
    fn render_diff(
        &self,
        previous: PreviousSection<'_, Self::Snapshot>,
    ) -> Vec<TextMessage>;

    fn matches_legacy_fragment(&self, _role: FragmentRole, _text: &str) -> bool {
        false
    }

    fn matches_retained_fragment(&self, _role: FragmentRole, _text: &str) -> bool {
        false
    }
}
```

`ID` is a persisted schema key and must remain stable. A snapshot contains only
what is necessary for comparison, not the entire source object. Section order is
deterministic and controls rendering order.

`render_diff` owns semantics. The coordinator does not inspect section-specific
JSON or guess how to express a transition.

## 4. Previous-state semantics

- **Absent:** no typed snapshot and no recognized retained fragment. Emit the
  section's initial representation when current state is present.
- **Unknown:** retained history appears to contain the section, but no compatible
  typed snapshot can be restored. Emit a conservative full representation once.
- **Known:** compare typed snapshots and emit only the section-owned delta.

Garbage or future-version snapshots degrade to unknown, not absent. This avoids
mistaking migration uncertainty for proof that the model knows nothing.

## 5. Persistence

Phase 1 stores a full snapshot under the existing session side state:

```json
{
  "world_state": {
    "v": 1,
    "sections": {
      "environment": {},
      "project_instructions": {},
      "dispatch": {},
      "tool_manifest": {}
    }
  }
}
```

Full snapshots are appropriate because `side` is already a compact current-state
snapshot, not an append-only rollout. The timestamped event log may later record
RFC 7386 patches for audit/reconstruction economy, but patches are not required
to land the section abstraction.

Snapshot serialization must be deterministic. Remove null object fields before
storage so future patch semantics can reserve null for deletion.

The snapshot is worker-owned. fleetd supplies live attempt and connection state;
blackopsd supplies collaboration and operational policy. Neither owns or
renders the model-knowledge baseline. After worker restart, the worker restores
the snapshot and reconciles it against fresh service policy before the next
model turn.

## 6. Retained-history reconciliation

Persistence alone can lie. A previous snapshot may be known while compaction or
provider-specific history rewriting removed the fragment that made the state
model-visible.

At restore and after compaction:

1. inspect the retained transport history for each section that provides a
   matcher;
2. if the snapshot is known and a matching rendered fragment remains, use the
   typed diff normally;
3. if the snapshot is known but no fragment remains, treat the section as
   absent for one emission;
4. persist the newly emitted snapshot baseline.

Matchers recognize section structure and stable markers, not exact full text.
They must remain conservative: false negative causes one redundant injection;
false positive can leave the model missing required context.

## 7. Composition and prompt caching

World State does not replace `CompositionStrategy`.

- Codex-shaped transports keep developer and contextual-user role separation.
- Vibe-shaped chat transports fold relevant state into the rebuilt system
  message.
- Section order remains stable across turns and providers.
- Deltas append only when state changes or retained-history repair is required.

The stable session ID remains the prompt-cache key. World State improves prefix
reuse by preventing full context resend and by keeping section ordering fixed.

## 8. Migration sequence

1. Add the section coordinator and persist full snapshots beside the existing
   cells, initially in shadow comparison mode.
2. Migrate environment and prove emitted output is unchanged.
3. Migrate dispatch scope/pins and delete `dispatch_emitted` after one tolerant
   restore cycle.
4. Migrate project instructions with loaded-path/content fingerprints and
   environment-triggered rediscovery.
5. Add tool-manifest and collaboration-capability sections.
6. Add retained-history reconciliation after transport snapshot inspection is
   shared across all three transports.
7. Remove `reference_context_item` only after old sessions restore through the
   legacy matcher path.

## 9. Verification contract

Tests must cover:

- stable section order and duplicate-ID refusal;
- absent, unknown, and known behavior;
- tolerant restore of missing, malformed, and future-version sections;
- environment delta parity with current behavior;
- unchanged sections emit nothing;
- a known snapshot with a missing retained fragment re-emits once;
- compaction followed by resume preserves required AGENTS and environment state;
- provider role/composition differences remain unchanged;
- old `reference_context` and `dispatch_emitted` sessions migrate without a
  duplicate first-turn fragment;
- prompt-cache tool/context ordering remains deterministic.

## 10. Relationship

- [Codexification](codexification.md) introduced `reference_context_item` and
  differential updates. This doc generalizes that shipped stage.
- [Dispatch prompt slots](dispatch-prompt-slots.md) still owns provider-specific
  placement and cadence.
- [Compaction designs](brodex-compaction.md) own history replacement. World
  State supplies the model-knowledge baseline that replacement must preserve or
  repair.
- [System memories runtime loading](../corpus/knowledge/system-memories-runtime-loading.md)
  owns memory discovery and payloads. World State may track which selected lens
  was rendered, not the memory corpus itself.
- [Worker protocol](worker-protocol.md) supplies live fleet policy and
  availability inputs. World State decides what change the model must see.
