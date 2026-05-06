# D2 + G1 fixes review

Commits `f45d70b..4799d0e` (4 G1 fixes + D2 inspect/describe_schema).

## Issues (fix-forward)

1. **Data-loading lives in `main.rs::inspect_extra_properties`, not in
   the providers.** D2 sidesteps D1's stub providers by passing
   pre-loaded `extra_properties: BTreeMap<String, String>` into the
   facade. The match is 90+ lines on `EntityRef` variant. This works
   for D2 alone, but D3's `bbox_find_paths` and `bbox_bundle_evidence`
   need the same lookup; they'll either (a) duplicate the match,
   (b) cross-call `inspect_extra_properties` (creates a cross-tool
   dep), or (c) finally push the load logic down into the providers
   (the right place per D1 review #1).
   Pick now: factor `inspect_extra_properties` into a shared
   `entity_loader.rs` module that all MCP tools call, or push into
   providers via a `&ProviderContext` arg. The current pattern
   doesn't scale to D3.

2. **`compact_label` still uses provider stubs.** Inspect's
   `render_edges` calls `providers::provider_for(...).compact_label(...)`
   to label edge endpoints. So a `KNOWLEDGE_FROM_SESSION` edge
   pointing at a knowledge entry shows `knowledge:abc12345` (the id),
   not the entry's title. Same for symbol, transcript, brofile, etc.
   The agentic LLM sees inline edges with bare IDs instead of human
   labels. Fix: when the provider's `compact_label` falls back to id
   components, the inspect facade should ALSO consult its
   data-loading path to derive a richer label. Or the providers gain
   data access (D1 fix #1) and `compact_label` returns actual titles.

3. **`render_text` is minimal**: just lists property keys + edge
   counts. No edge listings, no recommended-next-hops summary, no
   coverage. The structured JSON has all that info but the
   `text` field for inline LLM consumption is uninformative. Daystrom
   AgenticTools spike's text format was much richer (full edge
   listings with arrows, type info inline). Either:
   - Expand `render_text` to match the spike's text density.
   - Direct callers to use the structured fields and drop `text`
     from the response.

## Concerns

4. **`similar_refs` lookup walks `EdgeIndex::known_refs()` and
   filters by entity-type prefix + non-needle.** Returns up to 5.
   Reasonable; not levenshtein-similarity but close-enough by type.
   Note that `known_refs()` is O(n) over all edge endpoints — for a
   large EdgeIndex this is slow. Defer; flag.

5. **`describe_schema` lists 12 entity types and edge families.**
   Verify it picks up the F4-introduced `commit_author_name`/
   `commit_author_email` fields in commit's filterable-fields list.
   (Not yet inspected; flag if missing.)

6. **`render_properties` `Smart` mode truncates to 300 chars.** Same
   as daystrom's; correct. But the `summary_keys` allowlist is hard-
   coded `["name", "title", "status", "kind", "severity", "id"]`. A
   `KnowledgeEntry` has `category`, `scope`, `approval` — none in the
   summary keys. So `summary` mode for a knowledge entry shows only
   `id` + `title` (if title is in the keyset, which it is). Adding
   `category`/`scope`/`approval` would be useful. Subjective.

## G1 fix observations

7. **G1 fix #1 (commit author fields)** — added two new tantivy
   fields `commit_author_name` (TEXT) + `commit_author_email`
   (STRING). Bumped `INDEX_SCHEMA_VERSION` to `agentic-corpus-g1`
   (forces transcript reindex on next start). `COMMIT_BY_AUTHOR`
   placeholder edge dropped. ✓

8. **G1 fix #2 (incremental range guard)** — added
   `is-ancestor` check before using `since..HEAD`. On rebased
   history, falls back to full re-ingestion. ✓

9. **G1 fix #3 (head_fingerprint doc)** — comment added explaining
   the FileMeta.size overload. ✓

10. **G1 fix #4 (commit message cap)** — truncation to 16KB with
    `\n\n[... message truncated]` suffix. ✓

## Nits

11. **`inspect_extra_properties` returns `Result<Option<BTreeMap<...>>>`
    where `Ok(None)` means "entity ref didn't resolve to a known
    entity"** — but the function returns `Ok(Some(empty_map))` for
    Knowledge if the entry is missing... actually no, it returns
    `Ok(None)` correctly via `entry(id).map(...)`. OK; consistent.

12. **`render_text` writes `## {entity.ref_string}` as the markdown
    header.** Long entity refs (especially ProjectFile with 4
    components) make ugly headers. Use `compact_label` for the
    header instead.

13. **The `ok_status` field in error responses is implicit; the JSON
    has `"status": "error.bad_input"`.** Per design §4.4 this matches
    `error.<code>` format. Consistent.
