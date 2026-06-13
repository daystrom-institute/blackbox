# Large UI Section Extract Probe: DeepSeek V4 Pro

Public-safe accumulation of a private-project isolate probe. Raw private names
and paths are intentionally omitted.

## Probe Shape

- Provider/model: DeepSeek V4 Pro.
- Recipe: large UI section extraction.
- Target shape: a ~3,700-line routed Vaadin view with Guice injection.
- Gate: module compile with JDK 25.
- Result: compile passed after steering the bro to the host JDK. This exposed
  an orchestration setup gap: Java must be present in dispatched shell env before
  the probe starts.

## Sanitized Probe Response

The bro selected one high-confidence cohesion cluster:

- score `0.952`
- zero outbound calls
- 7 private methods
- 3 widget creators, 2 completion checks, 2 helpers
- 6 intra-class inbound calls
- source constructor had about 80 injected parameters
- candidate movement set included static constants, injected admin/service
  dependencies, and one mutable view-state field

The transform:

- moved 7 private methods and 17 fields into a new injectable delegate
- created an `@Inject` constructor on the delegate
- added an injected delegate field on the source view
- rewrote internal source call sites to `delegate.*`
- removed 9 now-unused constructor parameters from the source
- shrank the source by roughly 43KB
- created a delegate class of roughly 900 lines

Compile passed after using:

```bash
JAVA_HOME=/opt/homebrew/opt/openjdk@25 PATH=/opt/homebrew/opt/openjdk@25/bin:$PATH ./gradlew :webapp:compileJava
```

The bro reported only deprecation warnings.

## Orchestrator Measurements

Actual tool-use count from the event log:

- 30 total tool uses
- 19 `exec`
- 5 `file_read`
- 3 `shell_run`
- 2 `shell_poll`
- 1 `smart_read`

Error-bearing tool results:

- first `java.extractClass` call used a bare target name and failed package/path
  derivation
- later `bbox_slice_read` path resolution failed because the file path was
  interpreted relative to the wrong root

Independent compile rerun passed with the same JDK environment.

## Retro Response

The bro's retro summary:

- The recipe flow is sound: describe, cluster, survey, extract, apply, compile,
  cleanup, compile.
- The selected seam was appropriate: high score, zero outbound calls, delegate
  topology.
- DI was auto-detected.
- Constructor cleanup worked and removed dead injection parameters.
- Batching edit creation/merge/apply in one cell was clean.

Top friction points:

1. Cohesion cluster output was too large. The bro spent multiple cells reading
   and summarizing a bulky cluster payload. A compact/summary mode would reduce
   cell count.
2. `wrappers=true` was surprising. For private methods with no external callers,
   the transform rewrote source call sites directly instead of generating source
   wrapper methods. This compiled, but the contract needs to say this clearly.
3. Field access facts were too coarse. The cluster output exposed field touches,
   but not a read/write split. Moving the mutable field was a compile-safe
   choice here, but the bro had to infer ownership risk.
4. The default Java environment was wrong. `/usr/bin/java` was the macOS stub;
   the usable JDK was under Homebrew. This should be fixed once in dispatch env,
   not paid as a per-probe cell tax.

## Recipe Updates Suggested

- Configure project dispatch env so Java probes start with `JAVA_HOME` already
  available. Do not make every bro rediscover the host JDK.
- Require full source-root-relative paths for `java.extractClass` targets, or
  improve the transform contract so target-name-only behavior is obvious.
- Document wrapper behavior for private methods: `wrappers=true` preserves
  external API shape, but private moved methods may become direct delegate calls.
- Ask the bro to summarize cohesion clusters compactly before selecting a seam.
- Keep the final report JSON-first and treat missing fields as probe friction,
  not harmless formatting drift.

## Code Construct Candidates

- `analysis.cohesionClusters` compact mode: score, method count, outbound count,
  moved-field count, name hint, expected topology, and top candidate methods.
- Field access classification: read vs write per field per candidate cluster,
  with mutable-state ownership hints.
- Better `java.extractClass` target-path ergonomics or an explicit error that
  suggests the full source-root-relative path.
- Path-root normalization for file/slice reads inside code-mode cells.

## Probe 2 Rerun After Recipe/Env Fixes

The rerun started with `JAVA_HOME` already available, so the bro did not spend
probe cells rediscovering the host JDK. The full source-root-relative target
path also avoided the first probe's target-name failure.

Actual tool-use count from the rerun event log:

- 31 total tool uses
- 16 `exec`
- 5 `shell_run`
- 5 `file_read`
- 3 `shell_poll`
- 1 `smart_read`
- 1 `mcp__blackbox__bbox_note`

Independent compile rerun passed:

```bash
JAVA_HOME=/opt/homebrew/opt/openjdk@25 PATH=/opt/homebrew/opt/openjdk@25/bin:$PATH ./gradlew :webapp:compileJava
```

Rerun outcome:

- extraction succeeded
- source shrank by roughly 900 lines
- delegate class was roughly 900 lines
- cleanup removed dead constructor injection parameters
- final compile passed
- internal private-method callers were rewritten to delegate calls, with no
  source wrappers

Where the rerun still struggled:

1. Field declarations were not available through the item-summary surface, so
   the bro fell back to raw query patterns for field inspection.
2. A modifier query over-classified the moved fields as static/final; the
   transform's captured-variable findings later identified the mutable moved
   state correctly.
3. Compile validation used non-blocking shell polling and noisy build output,
   causing a missing shell-session error plus circular/truncated dump reads.
   Blocking compile mode worked cleanly afterward.
4. Post-transform verification repeated facts the transform could return
   directly: delegate injection point, delegate constructor injection, wrapper
   behavior, and internal caller rewrites.

Recipe-level fixes to carry forward:

- For compile gates, use a small helper or code-mode filter that runs the build
  in blocking mode and strips known noisy warnings from the returned summary.
- Do not pipe compile output through `tail`/`head` inside the shell command.
  Filter after capture so exit status and diagnostics stay intact.
- Treat pre-extract field modifier facts as provisional unless returned by a
  per-field fact surface or the transform's captured-variable findings.

Code construct candidates reinforced by probe 2:

- `code.fields({ file })` returning field name, type, modifiers, annotations,
  span, and owner class.
- `analysis.fieldClassification({ file, fields })` returning static/final,
  mutable instance, injected/provider, read-by, and written-by facts.
- Transform report fields for injection point movement, delegate constructor
  annotations, wrapper count, and internal caller rewrites.
- Harness shell-output handling that returns a bounded diagnostic tail without
  circular dump references.

## Probe 3 After Field Facts And Blocking Compile Recipe

Probe 3 reran the same large UI section extraction after adding first-pass
field fact surfaces and requiring blocking compile capture.

Actual pre-retro tool-use count:

- 25 total outer tool uses
- 16 `exec`
- 4 `file_read`
- 2 `smart_read`
- 2 `shell_run`
- 1 failed direct `code` tool call before switching to the namespace binding

Outcome:

- The bro used `analysis.cohesionClusters` and selected the same high-score,
  zero-outbound seam.
- It used `analysis.fieldClassification` before mutation.
- `java.extractClass` produced a clean result with `fixme_count=0`.
- The bro initially split extract/apply across cells and lost the stored
  changes payload, then recovered by running extract/apply in one cell.
- Both compile gates passed.
- Cleanup removed dead constructor injection parameters.
- Independent host compile passed with explicit JDK 25 env.

Probe 3 retro findings:

- `code.items` did not expose Java field declarations; probes should use
  `code.fields` when declaration spans or owner-class facts are needed.
- `analysis.fieldClassification` did not yet detect constructor-injected
  fields, so DI-heavy source fields appeared as `is_injected: false`.
- Large transform payloads were unreliable across `store()`/`load()` cells.
  The recipe should require `java.extractClass` and `edits.apply` in the same
  cell.
- The compile output was still noisy, but blocking mode preserved exit status.

Changes made after Probe 3:

- Implemented constructor-param injection detection in
  `analysis.fieldClassification`: `@Inject` constructor assignments of the
  form `this.field = param` now return `is_injected: true` and
  `injection_style: "constructor_param"`.
- Updated recipe/probe prompts to batch independent describe calls, use
  single-cell extract/apply, treat `code.fields` as the declaration surface,
  and avoid relying on `store()` for transform payloads.
- Filed generic gaps for transform payload persistence and Java field
  discoverability.

## Probe 4 After Constructor Injection Detection

Probe 4 reran the same unit after rebuilding/restarting the daemon with the
constructor-injection classifier and the updated recipe.

Actual pre-retro tool-use count:

- 25 total outer tool uses
- 9 `exec`
- 10 `file_read`
- 3 `shell_run`
- 2 `content_search`
- 1 `smart_read`

Outcome:

- Batched describe calls worked.
- `analysis.fieldClassification` reported all constructor-injected dependencies
  as `is_injected: true` with `injection_style: "constructor_param"`.
- The bro ran `java.extractClass` and `edits.apply` in one cell; no payload
  persistence failure.
- The transform remained clean with `fixme_count=0`.
- Cleanup removed the same dead constructor parameters and kept fields still
  used by the source.
- Independent host compile passed unfiltered.

Probe 4 retro findings:

- The constructor-injection construct is working.
- `fieldClassification` was sufficient for the pre-extract state/dependency
  analysis; `code.fields` remains for declaration spans and owner-class facts,
  not something every probe must call.
- If cohesion reports zero outbound calls and every moved method is private,
  the external caller survey can be skipped.
- Compile output remains the active protocol tension. The bare command
  preserves exit status but can dump generated-code noise; a filtered command is
  acceptable only if it preserves the primary command exit status.
- A future construct could preview "captured but not moved" dependencies before
  `java.extractClass` so the operator can decide whether constructor-param
  promotion is acceptable before applying.

Recipe updates after Probe 4:

- Allow filtered compile commands only with `bash -o pipefail` and a
  non-short-circuiting filter. Do not use `head`/`tail`.
- Require reports to include the unfiltered gate command even when the executed
  command used an output filter.
- Add the zero-outbound/private-method guard for skipping external caller
  surveys.

Remaining code construct candidates:

- `shell_run` post-capture filtering or summary that preserves primary command
  exit status without relying on shell-pipe discipline.
- Pre-extract captured-dependency projection for `java.extractClass`.
- Optional dry-run/preview mode for `java.extractClass` when a seam is less
  obviously safe than this one.
