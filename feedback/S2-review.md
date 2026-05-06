# S2 + F4 fixes review

Commits `57adba9..44664e5` (6 F4 fixes + S2 project-file indexing).

## Issues (fix-forward)

1. **Markdown edges are self-loops with no useful targets.** `derive_edges`
   in `index/project_files.rs:255` emits `LINKS_TO_FILE` /
   `LINKS_TO_SECTION` / `EMBEDS_CODE_FENCE` edges where `source ==
   target == chunk`. The design intent is the edge points at the
   actual link target (other file's chunk for `LINKS_TO_FILE`, the
   embedded code-block chunk for `EMBEDS_CODE_FENCE`, the target
   section for `LINKS_TO_SECTION`). As shipped, these edges add noise
   without traversal value. Either:
   - Resolve targets at index time (parse markdown link URL; resolve
     to absolute path; look up project_file_chunk by content hash —
     requires the index already to contain the target). Defer to a
     post-S4 phase since EdgeIndex needs to exist.
   - Stop emitting these edges until target resolution lands.
   The `NEXT_SECTION` edges (chunk N → chunk N+1 in same file) ARE
   correct and useful.
   Pick one; document the choice. Right now the `kind` field carries
   an edge-family label whose semantics aren't honored.

2. **`byte_start: 0, byte_end: 0` for non-markdown chunks.** The
   `placeholder_chunk` helper accepts byte offsets but `JsonChunker`,
   `TomlChunker`, `YamlChunker`, `PlainTextChunker` all pass `0, 0`.
   Only `MarkdownChunker` passes real offsets. The tantivy `byte_offset`
   field then carries 0 for every config/text chunk, and 0 for the FIRST
   markdown chunk too. Either:
   - Populate offsets for all chunkers (not hard — track byte position
     during the split loop).
   - Drop `byte_start/byte_end` from the `Chunk` struct if not used.
   Code-chunker (S3) will have real offsets from tree-sitter; the
   inconsistency between chunker types is a footgun.

3. **`current_head` is duplicated** between `entity_ref.rs::git_first_commit_for_path`
   and `project_files.rs::current_head`. Both shell out to `git`. S1
   review item #5 already flagged the proliferation — this is the
   second instance. When G1 lands (git ingestion needs MUCH more git),
   extract a `src/git.rs` module that owns all git invocations.

## Concerns

4. **Bootstrap arc is observable skeleton, not the actual mechanism.**
   Same pattern as F3's schema-migration-arc. Codex flagged this in
   the done note; it's the right v1 trade-off (workflow hook ops don't
   yet have index-state access). Document explicitly in the release
   notes: "project-bootstrap-arc currently records the migration shape;
   actual chunk emission runs in `bbox_project_register` →
   `index_registered_projects_standalone`. Wiring hook ops with index
   handles is deferred to a later phase, likely D1/D2 when the inspect
   surface needs to dispatch indexing as a workflow step."

5. **`is_supported_text_path` excludes common config extensions** —
   `.cfg`, `.ini`, `.env`, `.dockerfile`, `Dockerfile`, `Makefile`,
   `.lock` (Cargo.lock, package-lock.json). Some are deliberate skips
   (lockfiles); others might matter for a complete corpus. Defer; flag
   for later expansion.

6. **`serde_yaml` dep added to Cargo.toml** but not in the original
   declared deps list. Note in release notes alongside `tree-sitter-language-pack`
   coming in S3 so future dep audits know which phase introduced what.

## F4 fix observations

7. **F4 fix #1 (deactivate superseded artifacts)** — wired correctly.
   `mark_superseded` now removes from `workflow_registry`, deletes the
   workflow JSON from disk, calls into the packet store deactivation,
   and removes brofile JSON. Regression test
   `artifact_supersession_deactivates_workflow_registry_entry` passes.
   Good.

8. **F4 fix #2 (bound remote downloads)** — exceeded the spec. Ships:
   30s timeout, 10-redirect limit, scheme check (rejects redirects to
   non-http(s)), content-type assertion, content-length pre-check,
   AND streaming size enforcement (catches missing content-length).
   Synthetic oversized-response test included. Best fix in the cycle.

9. **F4 fix #3 (explicit discovery kinds)** — replaced strip-`s` with
   explicit map: `"workflows" → ArtifactKind::Workflow`. Clean.

10. **F4 fix #4 (lift workflow capability validation)** — pulled out
    of `BlackboxServer::new`. Both install paths use the lifted
    function. Clean.

11. **F4 fix #5 (supersedes_chain test)** — v1 → v2 → v3 case landed.
    Asserts chain accumulates. Good.

12. **F4 fix #6 (truncate locator errors)** — descriptions clipped to
    ~80 chars in error message. Good.

## Nits

13. **`MarkdownChunker` only handles h2 boundaries (`## `).** What
    about h1 / h3? A README starting with `# Title` and followed by
    `## Section A` produces two chunks (intro + Section A). A doc with
    nested `### Subsection` doesn't get its own chunk — buried inside
    the parent h2 chunk. Acceptable for v1; flag when a user complains.

14. **`is_binary` checks first 4096 bytes for any zero byte.** Won't
    catch UTF-16 (which has zeros for ASCII). Probably fine for
    English-language repos; flag if internationalization matters.

15. **JsonChunker's byte_start/byte_end are 0** AND the rendered chunk
    content is the JSON re-serialized via `to_string_pretty` of a
    single-key object. So a JSON file with `{"foo": 1, "bar": 2}`
    produces two chunks: `{"foo": 1}` and `{"bar": 2}`. That's fine
    semantically but the chunk doesn't carry the original source
    formatting. Edge case: re-running the chunker on an unchanged file
    might produce different `chunk_hash` if serde_json's pretty-print
    canonicalization differs from the source style. Verify
    content-hash dedup works on JSON files with non-canonical
    formatting.
