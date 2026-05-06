# S3 + S1 fixes review

Commits `e0d5ba2..72dfc66` (5 S1 fixes + S3 code chunking).

## Issues (fix-forward)

1. **`HAS_FIELD`, `IMPORTS`, `IMPLEMENTS_TRAIT` edges target synthetic
   `external_symbol_ref` entities that don't exist in the corpus.**
   The function builds an `EntityRef::Symbol` with the SOURCE chunk's
   `defn_hash` and a fabricated `qualified_name`. So these edges
   point at phantom entities — `bbox_inspect_entity` would return
   `not_found` for the targets. The asymmetry is real:
   - `CALLS` edges resolve through the project-wide symbol table to
     REAL symbol entities. ✓
   - `HAS_FIELD` / `IMPORTS` / `IMPLEMENTS_TRAIT` use synthetic refs
     with the source's defn_hash. ✗
   Two paths:
   - For in-project targets (e.g. `impl InspectableEntityProvider for
     KnowledgeProvider` where the trait is in this repo), look up via
     symbol table and use the real ref. Same as `CALLS`.
   - For out-of-project targets (e.g. `use std::collections::HashMap`,
     `impl Display for EntityRef`), don't emit a `Symbol` edge target
     — either skip the edge entirely OR introduce a virtual external
     entity type (`external:<crate>:<path>`). The former is simpler.
   The current synthetic approach is the worst of both: emits an
   edge that looks like it points somewhere but doesn't.

2. **No code-aware tantivy tokenizer.** S2's prompt called out that
   the design wants code-aware tokenization (`_`, `::`, `.`,
   camelCase split) on the `code_content` field. S3 emits code into
   `code_content` but uses tantivy's default tokenizer — same as
   `content`. So `bbox_search(query="KnowledgeStore")` falls back to
   the same matching as the prose field; identifier-split queries
   like `bbox_search(query="bbox_project_register")` won't split on
   `_` and may miss exact matches in code. Either:
   - Implement a custom tokenizer (small Rust struct conforming to
     `tantivy::tokenizer::Tokenizer`).
   - Document the deferral in `agentic-corpus-release-notes.md` so a
     later phase (likely H1 when search ranking gets serious) picks
     it up.
   Codex's done note didn't mention this gap.

3. **`call_names` regex matches `if (foo)`, `match (foo)`,
   `while (foo)` — flow-control keywords with parenthesized
   conditions look syntactically like calls.** They're filtered by
   `CALL_KEYWORDS` — verify the list covers all flow-control
   keywords across languages: `if`, `else`, `while`, `for`, `match`,
   `switch`, `return`, `unless`, `loop`, `do`, `case`, `await`,
   `async`, `try`, `catch`, `with`, `using`, `assert`, etc. Audit
   the list; missing keywords produce phantom CALLS edges to
   nonexistent symbol names that happen to coincide with control
   flow.

## Concerns

4. **`is_symbol_node` lists 18 tree-sitter node kinds** intended to
   cover all 9 languages. Some kinds match the wrong concept in some
   languages (`type_declaration` in Go ≠ `type_declaration` in
   TypeScript). Acceptable for v1 since `symbol_name`'s field-name
   lookup filters wrong matches. Flag for revisit when Tier B
   (Y-Rust etc.) lands per-language semantics.

5. **`fallback_name` (`first_identifier_after_keyword`)** is a token
   scan over chunk text. Will produce wrong qualified names for
   nested structures (`mod foo { mod bar { fn baz() }}` → bare name
   "baz" but qualified might be wrong). The `parents` stack in
   `collect_ast_symbols` takes care of qualification when AST
   navigation works; the fallback only fires when AST navigation
   doesn't yield a name. So this is a corner-case concern, not a
   default failure mode.

6. **`structure_symbol_specs` (language-pack path) AND
   `ast_symbol_specs` (raw AST path) are belt-and-suspenders.** If
   the AST walker yields specs, the structure walker is skipped; if
   AST yields nothing, fallback to structure. This is defensive but
   adds a code path that's rarely tested. Surface in done note: "I
   expected the AST path to handle all of [list of confirmed
   languages]; the structure fallback is dead code for those." If
   the structure path is genuinely needed for any in-set language,
   note which one.

7. **`tree-sitter-language-pack` plus 9 individual `tree-sitter-*`
   crates** is a meaningful dep surface. Cargo.lock grew by ~173
   lines. Each language's grammar pulls in build deps. Build time
   will take a noticeable hit. Confirm CI build time is still
   acceptable; flag if not.

## S1 fix observations

8. **S1 fix #1 (descriptions)** — verbatim port of the proposed
   text. Both bbox_project_register and bbox_project_list now carry
   substantial cuing text. Good.

9. **S1 fix #2 (git root reuse)** — verify that `register_path` no
   longer calls `git_root_for_path` twice. (Not yet read; flag if
   wrong.)

10. **S1 fix #5 (sync project tool docs)** — codex added a fifth
    commit beyond the 4 I asked for, syncing `tool_docs.rs` with
    the new descriptions. Good catch — the tool reference would
    have drifted otherwise.

## Nits

11. **`type_names` regex `\b([A-Z][A-Za-z0-9_]{2,})\b` requires
    ≥3-char Type names.** Misses `Vec`, `Box`, `Arc`, `Mutex`, `Rc`
    — common 2-3-char Rust types. Filtered by symbol-table presence
    so false positives don't leak; but reduces coverage. Drop the
    `{2,}` minimum to `{1,}`.

12. **`is_ident_char` is ASCII-only** (`ch.is_ascii_alphanumeric()`).
    Identifiers in Python and JavaScript can contain Unicode. Won't
    affect this repo's corpus; flag if a user's project has Unicode
    identifiers.

13. **`process(source, &config).unwrap_or_else(|_| ProcessResult::default())`**
    swallows tree-sitter-language-pack errors silently (only logs at
    debug). If `process()` errors consistently for a language, the
    fallback to direct grammar always fires, masking the underlying
    cause. Log at warn-level on first failure per language so it's
    visible in operations.
