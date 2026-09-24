code.query authoring guide (tree-sitter query syntax as code.query runs it)

VOCABULARY FIRST
  code.nodeKinds({ language, match? }) lists the loaded grammar's vocabulary:
    node_kinds       named kinds, written (kind)
    anonymous_kinds  literal tokens, written quoted: "fn", "+", "("
    supertypes       hidden abstract kinds, written (kind), plus (super/sub)
    fields           the grammar-wide field names, written field: (child)
  `language` is the name code.files / code.items report; code.query picks the
  grammar from the file extension, so the vocabulary is the one it compiles
  against. `match` is a case-sensitive substring filter. Vocabulary is not
  proof: a kind or field existing says nothing about whether your pattern
  encodes the structural question you mean.

SYNTAX FORMS (placeholders, not runnable examples)
  (kind)                     a named node of that kind
  (kind (child))             kind with a child anywhere among its children
  (kind field: (child))      child reached through a named field
  (kind !field)              kind with that field absent
  "token"                    an anonymous node; (kind "token") scopes it
  (_)                        any named node;  _  any node, named or anonymous
  (super)  (super/sub)       a supertype, or one subtype seen through it
  (ERROR)  (MISSING)         parse-error nodes; (MISSING kind) for a
                             specific missing node
  @name                      capture; one capture row per captured node
  ( ... )* ( ... )+ ( ... )? quantifiers;  [ a b ]  alternation
  .                          anchor: first/last/adjacent named child
  (#eq? @cap "text")  (#not-eq? ...)  (#match? @cap "regex")
  (#not-match? ...)  (#any-of? @cap "a" "b")   text predicates, applied
                             host-side; only matching rows come back

LIMITS OF THE METADATA
  fields is a flat grammar-wide list, not a per-kind table: a field that
  exists can still be impossible under a given kind. Compilation checks that
  (an "Impossible pattern" error), and so do fixture tests.
  supertypes[].subtypes is null when the grammar carries no subtype table
  (older tree-sitter ABIs). Null means unknown, never "no subtypes"; the
  supertype name itself is still a legal query node.

RESULTS AND ERRORS
  An invalid query fails the call with "invalid tree-sitter query for
  language <lang>: Query error at row:col. <reason>". Reasons: Invalid node
  type, Invalid field name, Invalid capture name, Invalid predicate,
  Impossible pattern, Invalid syntax. Fix the query; nothing matched.
  A valid query that matches nothing returns captures: []. An empty result
  does not prove the construct is absent: the pattern may encode a different
  shape than intended. Check the pattern against a file where you know the
  construct exists before trusting an empty sweep.
  Each capture is { capture, kind, text, span }; span is hash-anchored, so
  re-run the query after any write to the file.

SCOPE AND LIMITS
  within: { byte_start, byte_end } keeps matches intersecting that byte
  range of one file (single-file calls only; out-of-bounds ranges error).
  files: [...] runs one query host-side over many files and returns a flat
  captures array plus a per-file roll-up; a file that fails reports
  { file, error } and the call fails only when every file fails. One file
  stops at 5000 captures (truncated: true). A batch stops near 20000
  captures with aggregate_capped, files_scanned, files_total and a hint:
  narrow the pattern or the file set and re-run.
