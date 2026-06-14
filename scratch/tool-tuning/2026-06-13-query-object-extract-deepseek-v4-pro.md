# Query Object Extract Probe: DeepSeek V4 Pro

Public-safe accumulation of a private-project isolate probe. Raw private names
and paths are intentionally omitted.

## Probe Shape

- Provider/model: DeepSeek V4 Pro.
- Recipe: query-object extraction.
- Target shape: a small Java admin/data-access class with three public
  read/query methods and one public write/save method.
- Goal: extract the read/query concern into a new query object, preserve source
  public wrappers, leave write behavior in the source class, and avoid caller
  migration.
- Gate: module compile with JDK 25.

## Outcome

- Extraction applied successfully.
- Independent host compile passed after the bro completed.
- The source class kept public wrappers for all moved query methods.
- The write/save method stayed in the source class.
- The new query object used constructor injection for the shared query context
  provider.
- `dependency_projection` reported `external_injection`, one injectable
  constructor param, and no non-injectable params.
- `previewOnly` was not used; the seam was simple and clean.
- No manual surgery and no failed tool-result cells.

Actual pre-retro tool-use count:

- 14 total tool uses
- 3 `exec`
- 2 `shell_run`
- 7 `smart_read`
- 2 `file_read`

Independent compile rerun:

```bash
JAVA_HOME=/opt/homebrew/opt/openjdk@25/libexec/openjdk.jdk/Contents/Home \
PATH=/opt/homebrew/opt/openjdk@25/libexec/openjdk.jdk/Contents/Home/bin:$PATH \
./gradlew :webapp:compileJava
```

Result: `exit_code=0`, `BUILD SUCCESSFUL`.

## Retro Findings

The core transform path worked. The useful findings were cleanup and analysis
ergonomics:

- `java.extractClass` copied imports into the target more broadly than needed.
  This was harmless for Java compile but would matter under strict unused-import
  checks.
- Method deletion left a run of blank lines in the source class.
- The generated delegate field annotation did not match the source file's
  annotation placement style.
- `analysis.references` returned a payload large enough that the bro read the
  harness dump. For this recipe, counts and files are usually enough; examples
  should stay out of the main text unless needed.
- Caller-role classification was manual from package/path/context snippets. It
  was cheap for this tiny target, but will not scale for broader query families.
- The filtered compile path worked and correctly preserved the primary command
  exit status.

## Recipe Changes To Carry Forward

- Add a known-artifacts section for query-object extraction:
  - unused imports may remain after extraction;
  - blank-line gaps may remain where methods were removed;
  - generated delegate fields may have non-idiomatic annotation placement.
- Tell bros to summarize `analysis.references` as counts/files first; read
  examples only when caller classification depends on them.
- Treat post-extract import pruning and whitespace/style cleanup as optional
  cleanup, not a reason to rerun a successful transform manually.

## Code Construct Candidates

- Post-extract cleanup bundle:
  - remove unused imports;
  - collapse deletion whitespace;
  - normalize generated field annotation placement.
- Compact reference-summary mode for `analysis.references`.
- Caller-role classification facts for larger query-object targets.

## Rerun After Cleanup Utilities

After installing the cleanup changes, the same provider/model reran the same
unit of work in a fresh disposable worktree.

Outcome:

- The extraction applied successfully again.
- The source delegate field now rendered with annotation on its own line.
- Method deletion no longer left the wide blank-line gap observed in the first
  run.
- A stale source-side static import still survived the main extraction and had
  to be removed in a follow-up isolate edit.
- The new extracted class still had minor formatting residue: missing blank line
  before the class declaration, a double blank line after the constructor, and
  one indentation drift inherited from the moved method body.
- Compile passed before cleanup and after cleanup.

Additional tool feedback:

- The bro first called `shell_run` with `output_filter` as strings and received
  a validation error requiring arrays.
- The corrected `output_filter: { stdout: [...], stderr: [...] }` call then
  stranded the turn: blackbox reported the tool as running, but host inspection
  showed no Gradle/harness child process and no persisted tool result.
- Recovery used shell-native filtering via `bash -o pipefail -lc './gradlew ...
  2>&1 | egrep ...'` through bare `shell_run`, which returned normally.

Follow-up construct candidates:

- Add source-side organize-imports cleanup into `java.extractClass` after wrapper
  generation, including single-member static imports.
- Add Java target/source whitespace normalization after generated create/edit
  application.
- Investigate `shell_run.output_filter` result delivery; the stuck call is a
  harness/tool defect or contract quirk from this campaign, not recipe content.
