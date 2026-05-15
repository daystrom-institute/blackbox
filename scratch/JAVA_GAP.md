# Java Refactor Agent Gaps & Wishlist — swing 38 (ProductionReadingAdmin Angle B)

Fresh log. Predecessor closure work (G1–G21) landed. New swing surfacing
gaps under the next stress test: extract `ProductionReadingRepository`
interface from `ProductionReadingAdmin` (1800 LOC, 86 methods, biggest
god class in the codebase), then `rename_java_symbol` Admin →
AdminImpl across 111 files / 223 edits.

Result: **only 1 manual step needed** — `git mv Admin.java → AdminImpl.java`.
Both raw plan kinds (`extract_java_interface`, `rename_java_symbol`)
otherwise produced compile-green output mechanically.

---

## G22 — codex bridge still rejects `bbox_note` / `bbox_refactor_plan` / `bbox_refactor_status` / `bbox_knowledge` under old namespace

**Severity:** Caution — agents that require those tools fail at the
precondition check; raw plan calls work fine under the new
`mcp__blackbox-ops__*` namespace.

**What happened:** I dispatched `java-class-dependency-graph` v2 and
`java-extract-interface` v2 against ProductionReadingAdmin. Both
self-blocked on codex with:

> "The required primitives are not exposed in this session:
> bbox_knowledge, bbox_refactor_status, bbox_refactor_plan,
> bbox_refactor_run, bbox_note."

The dispatch's `merged_filters.allow` lists the old `mcp__blackbox__*`
namespace; the active server is `mcp__blackbox-ops__*`. Some tools
(`bbox_code_symbols`, `bbox_code_query`, `bbox_code_node_describe`)
appear to be bridged backward-compatibly. The heavier refactor +
notes primitives are not.

Two failed agent runs (~330K input tokens) before falling back to
raw `bbox_refactor_plan(...)`, which worked first try.

**Wishlist:** complete the namespace bridge for all
`mcp__blackbox__*` agent-allowlist entries, OR rewrite agent
manifests to reference `mcp__blackbox-ops__*`. Today the partial
bridge produces agent-shaped failures depending on which tools each
prompt template touches and in which order.

---

## G23 — Agent prompt-template engine doesn't resolve missing optional placeholders

**Severity:** Trivial — extra defensive call with empty values
works around it.

**What happened:** dispatching `java-extract-interface` with the
required-only schema fields produced:

> "There is also a hard input precondition failure: the dispatch
> still contains unresolved placeholders for `{{impl_name}}`,
> `{{item_names}}`, `{{migrate_call_sites}}`, `{{migrate_callers_in}}`,
> `{{acknowledge_public_api_change}}`, and `{{validation_cmd}}`."

The agent schema marks these as optional (no `required` flag); the
templating layer surfaces them as literal `{{name}}` tokens in the
expanded prompt and the model bails on the placeholder text rather
than treating it as empty/default.

**Wishlist:** the prompt-template expander should substitute missing
optional placeholders with empty string / null / `[]` per the
parameter's schema type, OR strip the surrounding line if the
placeholder is the only content. Mustache and Liquid both behave
this way by default; the current expander appears to leave the
literal `{{X}}` token in.

---

## G24 — `rename_java_symbol` requires `item_names` parameter; error message doesn't suggest the right key

**Severity:** Trivial — wrong parameter shape produces a generic
error.

**What happened:** my first dispatch passed
`module_name: "ProductionReadingAdmin"` and
`new_text: "ProductionReadingAdminImpl"`, omitting `item_names`. The
planner refused with:

> "item_names is required for rename_java_symbol"

The error is true but unhelpful — the docs and other plan kinds
accept `module_name` as the symbol-to-rename, and a Rust speaker
might reach for `module_name` first. Once I passed
`item_names: ["ProductionReadingAdmin"]` it worked.

**Wishlist:** either make `module_name` an accepted alias for
`item_names[0]` on rename_java_symbol (since this plan kind only
ever renames one symbol at a time), or update the error to suggest
the right shape: `error.bad_input(code=missing_item_names, hint="pass
item_names: [\"<symbol>\"]")`.

---

## G25 — Default `item_kinds` filter on `rename_java_symbol` silently undercatches references

**Severity:** Caution — produces a partial rename that compiles
trivially but leaves callers stale.

**What happened:** my first rename dispatch passed
`item_kinds: ["class_declaration", "type_identifier"]` (an
overly-restrictive narrowing intended to be safe). The plan reported
**1 file changed** — only the declaration in the source file
itself, none of the 110 cross-file callers.

The same call with `item_kinds` omitted reported **111 files /
223 edits** — the full transitive rename.

The restrictive filter silently dropped `method_invocation`,
`field_access`, `import`, `method_reference`, and other reference
kinds that the rename needs to cover. A `type_identifier` filter
alone catches type-position references (variable decls, params,
return types) but misses imports and methods invoked on the renamed
type's instance fields.

**Wishlist:** when an operator passes `item_kinds` that excludes
the reference categories the rename absolutely needs (`import`,
`type_identifier`, `method_invocation`, `field_access`,
`method_reference`), refuse with
`error.bad_input(code=rename_item_kinds_incomplete, missing=[…])`
listing the kinds that would produce an incomplete rename. Or
silently widen to include those kinds while honoring the
inclusions for the optional ones.

Until then, document loudly: **rename_java_symbol's
`item_kinds` filter is dangerous** unless the operator has audited
which kinds are needed for full coverage.

---

## G26 — `bbox_refactor_apply` refuses on dirty git when applying a follow-on plan in the same session

**Severity:** Trivial workaround (commit between steps).

**What happened:** I ran `extract_java_interface` (apply 1) which
wrote to `ProductionReadingAdmin.java`. Then I generated and tried
to apply `rename_java_symbol` (apply 2) on the now-dirty Admin
file. Apply refused:

> "refusing to apply ProductionReadingAdmin.java: file is dirty in
> git; pass allow_dirty_worktree=true to override"

The Apply 2 plan was generated AFTER Apply 1's writes and used the
post-Apply-1 hash for its dirty-file check — but the *git* check
fired first and used "modified-vs-HEAD" rather than the plan's
hash. So Apply 2 saw a clean hash diff but a dirty git diff, and
the git check vetoed it.

For multi-step swings (extract + rename + migrate) inside one
session, this forces the operator to commit between every step,
which is awkward when the steps are mid-experiment.

**Wishlist:** when the plan's recorded `original_sha256` matches
the file's current sha256 (i.e., apply 1's output is still there
intact), skip the git-dirty check. The dirty check exists to
prevent overwriting hand-edits; if the previous apply wrote exactly
the bytes the new plan expects, there are no hand-edits to lose.

Alternatively, document a single-flag opt-out for the common
"chain of automated plans without committing between" workflow.

---

## G27 — `file_rename_advisory` is in the plan but absent from the apply response

**Severity:** Caution — silent operator action required.

**What happened:** `rename_java_symbol` on a top-level class
correctly emitted a `file_rename_advisory: [{from:
"Admin.java", to: "AdminImpl.java"}]` field on the saved plan JSON.
The `bbox_refactor_apply` response, however, only echoed
`files_written: [...]` and `validations: [...]` — no mention of the
advisory.

Result: I applied the plan, ran compile, got 30+ errors all
downstream of "class ProductionReadingAdminImpl is public, should be
declared in a file named ProductionReadingAdminImpl.java." Had to
inspect the saved plan JSON manually to discover the documented
advisory.

**Wishlist:** apply response should bubble `file_rename_advisory`
from the plan into the response payload so operators see it
immediately. Stronger: auto-execute the move with `git mv` (or
filesystem rename when not in a git repo) as part of the apply,
gated behind `auto_apply_file_rename: true` (default true seems
correct — the move is mechanically derivable from the rename and
java compiler hard-requires it).

---

