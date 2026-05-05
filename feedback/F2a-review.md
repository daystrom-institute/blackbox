# F2a review — eval suite skeleton

Commit `5542376`, `eval/check.rs` (339 LoC) + 30 JSON manifests + main.rs hook.

## Issues (fix-forward)

1. **`entity_type_hint` is `String`, not validated.** A typo
   (`entity_type_hint: "smbol"`) loads without complaint. F2b will resolve
   hints into real EntityRefs and a silent typo wastes a debugging round.
   Parse via `EntityType::from_prefix(&hint)` during `load_manifests`
   and surface invalid hints as errors. The grammar exists in F1 — use it.

2. **Manifest `id` is duplicated between filename stem and JSON body, with
   no consistency check.** `exact-symbol-knowledge-store.json` carrying
   `"id": "exact-symbol-knowledge-store"` is a drift risk. Either derive
   the id from the filename at load (and drop the JSON field), or add a
   test assertion that they match.

3. **`MANIFEST_SOURCES` is a hand-maintained 130-LoC tuple array.** Every
   new manifest requires editing this array. For v1 this is acceptable
   because manifests are part of the codebase. But F2b lands resolved
   EntityRefs that may regenerate manifests, and any later phase that
   adds manifests programmatically forces a refactor. Either:
   - Replace with `walkdir` over `eval/queries/` at startup (loses
     compile-time guarantee), OR
   - Generate the array from a build script reading the directory.
   Flag for F2b to revisit; don't refactor now.

4. **`RequiredEvidence.value: serde_json::Value` is loose.** `edge_family`
   takes a string, `entity_set` takes a string array, `path` takes
   something else (TBD). Today nothing validates the value matches the
   kind. F2b's checkers will need to interpret per-kind; without a typed
   variant per kind, drift is silent. Either model as a tagged enum
   (`#[serde(tag = "kind", content = "value")]` over an
   `Evidence::EdgeFamily(String) | Evidence::EntitySet(Vec<EntityType>) | Evidence::Path(...)`),
   or add a `validate()` method called during load.

## Concerns

5. **`pass_classifier: String` + `checker_by_name` lookup table.**
   Adding a checker requires three coordinated edits (function impl,
   match arm, JSON reference). The `stub_checker!` macro tightens one
   side; consider similar treatment for the dispatch table or accept
   the coordination cost.

6. **`forbidden_stale_answers: Vec<String>` is prose.** F2b's checkers
   will need to interpret it — either resolve into structured
   forbidden-entity-refs, or pass through to the LLM prompt as soft
   guidance. Document the contract when F2b lands; don't let it stay
   ambiguous.

7. **The `decision-*` filenames don't match the `StaleDecisionLookup`
   class name.** Enum is `StaleDecisionLookup`, JSON serializes as
   `stale_decision_lookup`, files are named `decision-*.json`. No
   incorrectness, but a reader scanning the directory will guess
   `stale-decision-*` to grep. Either rename files or add a comment in
   `MANIFEST_SOURCES` explaining the convention.

## Nits

8. **`StaleDecisionLookup` variant name.** Other classes are
   noun-shaped (`ExactSymbol`, `ConceptualDesignDoc`); this is
   verb-ish. `StaleDecision` would parallel.

9. **Top-level `check_pass` function (line 182)** exists only as a
   compile-time signature anchor. Worth a `///` doc comment explaining
   that real dispatch goes through `checker_by_name`.

10. **No assertion that ONLY 5 classes appear.** A new variant added to
    `QueryClass` without a corresponding test update would silently
    pass the count assertions. Add `assert_eq!(class_counts.len(), 5)`.

11. **`CheckPassFn` type alias used only in `checker_by_name`.** Could
    be inlined; the alias adds a layer of indirection for one site.
    Subjective.
