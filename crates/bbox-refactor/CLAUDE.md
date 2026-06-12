# bbox-refactor — the refactor substrate (two consumers, one future)

Daemon-independent by invariant: dependencies point down (corpus-core,
chunker, bbox-lsp, external) and never at `blackbox`. Two consumers share
this engine, deliberately without a behavioral fork:

- The daemon's v1 MCP adapters (`bbox_refactor_*` plan kinds, refactor runs)
  — **legacy, on the kill list** (decision af3c4783; refactor-tools-v2 §7
  strangler). Keep them working; do not grow them. The 100+ plan-kind
  catalog is CLOSED: new capability does not get a new `kind`.
- bro-harness cell bindings (`code.*`/`edits.*`/`lsp.*`) — the future. New
  capability lands as pure, data-returning substrate functions (the `facts`
  module pattern) or changes-returning transforms, consumed by bindings.

## The facts charter (load-bearing for the container test)

`facts` is pure functions of file bytes: no LSP, no daemon state, **no
writes**. The harness-native invariant — put the harness in a container and
refactor still works with the daemon absent — depends on this module staying
that way. Write machinery lives separately (`write_atomic` /
`write_atomic_noclobber` / `apply_text_edits`), kept verbatim from v1 because
the cell choke point reuses it.

## Footguns with history

- **Hash-check ordering**: when a caller supplies an expected content sha,
  verify it BEFORE interpreting byte ranges against a fresh parse. A drifted
  file must fail as `stale_span`; checked after, it surfaces as a confusing
  structural miss ("no function_item at range") against the new tree. This
  bit during development; the ordering is now part of the function contracts.
- **noclobber semantics**: `write_atomic_noclobber` closes the create TOCTOU
  at the rename. On its failure the target was never touched — callers must
  NOT push it into their rollback list.
- Grammars come from bbox-chunker (`ts_language_for_name`); item inventory
  walks are per-language (rust/java/generic fallback). `ParsedSource` is
  crate-private on purpose: expose derived data, never parse state.
- tree-sitter query iteration needs `streaming_iterator::StreamingIterator`
  — the cursor's matches are not a std Iterator.

## Sibling boundary

`bbox-macros` (the declarative macro layer) is folding down into cell
recipes + the function store (pressure-test doc §6.6); its Java emission
sidecar is the salvage. Don't add new coupling from here to there.
