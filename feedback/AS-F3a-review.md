Issues:
1. `AgentRegistry` claims pinned/superseded support, but `ArtifactCatalog` still stores one artifact and one metadata file per `(kind, name)`. `reviewer@v1` cannot be retrieved after v2 overwrites it; tests were weakened to pin `@v2`, so true history is untested.
2. `load_manifest(&entry.name)` ignores the listed entry/version/path. If the catalog later supports multi-version records, every version for the same name will parse the active artifact's manifest.
3. `list_with_superseded_shows_all_when_multiple` does not assert superseded behavior. It installs only one reviewer record and accepts `len() >= 2`, so it would pass without history support.
4. `AgentRecord.metadata.supersedes` is always `None`; registry records lose the direct supersedes edge even when artifact metadata has it.
5. `parse_name_or_ref` accepts empty names, `agent:`, `@v2`, `name@v`, `name@v0`, and non-numeric versions. Decide whether invalid refs return `BadInput` or `None`, but do not silently normalize them.
6. `embedding_pending` is `false` when manifest parsing fails because it uses `is_some_and`. Invalid/degraded manifests should surface as degraded/parse error or at least not look fully embedded.
7. Registry filtering drops agents with unparsable manifests from cost/provenance filtered lists without exposing the parse problem.

Nits:
8. `AgentRef` import in `registry.rs` is unused.
9. `cost_class = manifest.as_ref().and_then(|m| Some(m.cost_class))` should be `map`.
10. Consider returning typed provenance enum instead of raw strings if this is becoming an API surface.
