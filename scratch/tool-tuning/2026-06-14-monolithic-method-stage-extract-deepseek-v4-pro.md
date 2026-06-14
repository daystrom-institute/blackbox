# Monolithic Method Stage Extract Probe: DeepSeek V4 Pro

Public-safe accumulation of a private-project isolate probe. Raw private file,
class, package, and business names are intentionally omitted.

## Probe Shape

- Provider/model: DeepSeek V4 Pro.
- Recipe: monolithic method stage extraction.
- Target shape: an ~826-line Java/Vaadin dialog-builder method inside a large
  UI class.
- Candidate concern: a controls sub-surface inside the long method.
- Gate: module compile with JDK 25.
- Probe task: `b37f3d64-a3ed-4e3a-928e-d984b4bfcafa`.
- Retro task: `04d7173d-cc44-47f7-a987-c34780bb1020`.
- Session: `d3aec229-3989-40d1-ab40-1f1e76dc6159`.

## Outcome

No extraction was applied.

The bro found an existing lower-level Java code-block extraction primitive:
`extract_java_code_block_to_method`. That is useful: the monolithic-method
recipe is not completely unsupported, but it currently lives behind the generic
refactor-plan surface rather than the `java.*` code-mode namespace.

The selected logical concern was not a single clean block. It was spread across
four interleaved blocks inside the long method, separated by unrelated setup.
The bro then tried the most self-contained contiguous sub-region. The planner
refused with `error.multi_return_needs_record`: six locals declared in the
candidate block were read after the block. The planner recommended introducing
a record/result object, which would have required manual surgery and was
therefore correctly rejected by the probe protocol.

Baseline compile passed independently after the probe:

```bash
JAVA_HOME=/opt/homebrew/opt/openjdk@25 PATH=/opt/homebrew/opt/openjdk@25/bin:$PATH ./gradlew :webapp:compileJava --console=plain
```

Result: `BUILD SUCCESSFUL`.

## Measurements

Actual pre-retro tool-use count from the event log:

- 35 total tool uses
- 14 `file_read`
- 10 `exec`
- 2 `todo_write`
- 2 `shell_run`
- 2 `shell_poll`
- 2 `content_search`
- 1 `smart_read`
- 1 `mcp__blackbox__bbox_knowledge`
- 1 `list_dir`

Error-bearing tool results:

- wrong-root file read before switching to the worktree-rooted path
- namespace confusion: attempted `mcp__blackbox__java_describe` instead of the
  bare `java.describe` code-mode namespace
- attempted to search harness dump paths from inside the restricted worktree
- one swapped start/end line read
- first code-block extraction attempt omitted the required helper name
- second code-block extraction attempt refused with
  `error.multi_return_needs_record`

## Retro Findings

The bro's retro reported 20 code-mode cells, 4 errors, and 2 retries. It called
out three blocking construct gaps:

1. No auto-generated record/result-object companion for multi-live-out method
   extraction.
2. No non-contiguous-region extract-helper transform for interleaved UI-builder
   concerns.
3. No automated method-body region analysis; concern boundaries, live-outs, and
   captures were manually inferred from raw slices.

It also noted avoidable workflow friction:

- plan-kind discovery was too broad and produced large dumps;
- code-mode namespace bindings are easy to confuse with MCP tool names;
- simple identifier location was faster and cleaner through content search than
  a broad tree query;
- long method reading should start with outline/summary, then bounded ranges.

## Recipe Changes Applied

The private monolithic-method recipe and prompt were updated after the probe:

- add a **contiguity gate** before transform attempts;
- add a **live-out gate** before transform attempts;
- document the lower-level `bbox_refactor_plan` path for
  `extract_java_code_block_to_method`;
- tell probes to stop instead of hand-building a result object;
- remind code-mode users that `java.*`, `analysis.*`, `code.*`, `edits.*`, and
  `lsp.*` are bare namespace globals, while `bbox_*` tools remain under
  `tools.mcp__blackbox__*`;
- prefer outline plus bounded range reads over full-method dumps.

## Code Construct Candidates

- `java_method_body_region_analysis`: partition a long method into
  concern-labelled contiguous regions with read-before, live-out, field
  read/write, lambda/listener capture, and early-exit facts.
- `java.extractMethodCodeBlock`: code-mode namespace binding around the
  existing plan kind, returning edits-compatible payloads and focused contract
  text.
- `auto_generate_java_record_from_block_outputs`: generate and thread a small
  result record for multi-live-out block extraction.
- `extract_java_discontiguous_regions_to_method`: guarded extraction from
  multiple ordered ranges when a logical concern is interleaved with unrelated
  setup.

Generic substrate gap notes were filed for these candidates:

- `gap-df4938f7`: Java record bundles for multi-live-out extract-method regions.
- `gap-d8699272`: Java method-body region analysis.
- `gap-49e2ce33`: discontiguous Java region extraction.
- `gap-762ec9f7`: code-mode Java extract-method binding.

## Next Action

Do not rerun this exact probe yet. The next useful move is code/recipe work:
either implement method-body region analysis first, or add the `java.*`
extract-method binding plus a live-out preview/report before trying another
monolithic method.
