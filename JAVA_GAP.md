# Java Refactor Agent Gaps & Wishlist — validation pass 2

The G1–G19 closure work landed. I re-ran the same swing (extract
NonRoutineFlaringAndVentingService from NonRoutineFlaringAndVentingAdmin,
13 methods, ~919 LOC of workflow + queries) mechanically to validate.

`webapp:compileJava` GREEN after **4 hand-fix edits** — down from **7
edits** on the pre-closure run. Net mechanization: ~57% of remaining
manual work moved into the tooling.

Working swing: NonRoutineFlaringAndVentingAdmin → 458 LOC (-69% from
1477) + NonRoutineFlaringAndVentingService 1138 LOC. Branch
`bloat-swing-37` in worktree at
`.claude/worktrees/bloat-37`.

---

## Confirmed closed by validation pass (8 gaps)

| Gap | Evidence |
|---|---|
| **G5** — leave delegate wrappers on source for moved publics | `source_delegate_wrappers=true` generated 5 wrappers on Admin (save, remove, getSummary, fetchLatest, getProductionDate) — no manual additions needed |
| **G7** — Guice-incompatible auto-wiring | Plan refuses with `guice_field_injection_detected` when `wiring_mode` is unset. Explicit `wiring_mode=guice_field_inject` → Admin gets `@Inject private NonRoutineFlaringAndVentingService field;` (no synthesized null-capturing ctor) |
| **G12** — codex namespace prefix mismatch | Codex-backed dispatch reached the plan stage. (Still hits other issues — see below — but not the namespace problem.) |
| **G14** — public-static external_call FIXME noise | Subsumed by G19 fix: call sites are now class-qualified at write time, FIXME comments not emitted |
| **G15** — apply ignored cwd / wrote to plan-time paths | Apply refuses with `cross_worktree_apply` error and lists the cwd vs plan_path mismatch. New `cwd` parameter required for in-worktree applies; `force_path=true` available for explicit override |
| **G16** — annotation references missing from import walker | `import org.jetbrains.annotations.Nullable;` auto-added to target |
| **G18** — inner-type `::new` not qualified | `Records.mapping(NonRoutineFlaringAndVentingAdmin.NonroutineFlaringVentingDetailsRecord::new)` written with full qualifier |
| **G19** — cross-class public-static call not qualified | 2 call sites of `nonRoutineFlaringAndVentingBaseQuery(...)` rewritten to `NonRoutineFlaringAndVentingAdmin.nonRoutineFlaringAndVentingBaseQuery(...)` |

---

## Still partial / unclosed after this pass (3)

### G13 — `@Slf4j` annotation propagated but lombok import dropped

**Evidence:** the generated target carries `@Slf4j` on the class
declaration — propagation works — but `import lombok.extern.slf4j.Slf4j;`
is missing from the import block. Compile error:

```
NonRoutineFlaringAndVentingService.java:52: error: cannot find symbol
@Slf4j
 ^
  symbol: class Slf4j
```

**Likely root cause:** G13's annotation-propagation pass writes the
`@Slf4j` token directly, and G16's annotation-import walker only
walks annotation references it sees inside *method bodies and
signatures* — not class-level annotations the propagator just
emitted. The two passes don't talk.

**Fix shape:** when the propagator emits a class-level annotation,
queue the annotation's FQCN for import emission in the same pass.
Equivalently: re-run the import walker AFTER the propagator runs so
the new annotation is visible to it.

### G17 — Java record component bare-access not rewritten cross-class

**Evidence:** 4 sites in the moved code still read `.r`, `.site`,
`.plant` as bare fields on a record returned from a method whose
type is a record on the source class. Compile error:

```
NonRoutineFlaringAndVentingService.java:259: error: r has private access in NonroutineFlaringVentingDetailsRecord
NonRoutineFlaringAndVentingService.java:267: error: site has private access in NonroutineFlaringVentingDetailsRecord
NonRoutineFlaringAndVentingService.java:267: error: plant has private access in NonroutineFlaringVentingDetailsRecord
NonRoutineFlaringAndVentingService.java:772: error: r has private access in NonroutineFlaringVentingDetailsRecord
```

**Likely root cause:** the cross-package "bare-field → getter"
rewrite covers POJOs with `getX()` accessors but not records, whose
accessors are `x()` (no `get` prefix, same name as the component).

**Fix shape:** extend the rewriter to recognize the receiver's
declared type as a `record_declaration`, look up the record's
component name list, and rewrite `param.component` →
`param.component()`. Records' private backing fields aren't
package-accessible, so the rewrite triggers for any cross-CLASS
extract (not just cross-package).

### G7 cosmetic — stale `mutable_capture` FIXMEs above target fields

**Evidence:** the generated target file has 7 FIXME comment blocks
above each `private final X field;` declaration:

```java
// FIXME: mutable capture `dslContextProvider` (source field is non-final). Promoted to `final` constructor param — value snapshotted at construction.
//   resolutions: use Supplier<Provider<DSLContext>>, shared holder, or keep on source and access via reference.
private final Provider<DSLContext> dslContextProvider;
```

Those FIXMEs were written for the `wiring_mode=constructor_args`
failure mode (snapshotted null at ctor time). With
`wiring_mode=guice_field_inject` the ctor is `@Inject`-annotated
and Guice resolves the dependency at injection time — no
snapshotting happens. The FIXMEs are misleading noise.

**Fix shape:** when `wiring_mode` is one of the `guice_*` modes,
suppress the mutable-capture FIXME comments; the failure mode they
warn about doesn't apply. Optionally swap them for a one-line
"Guice-injected, see source class for binding" comment, but I'd
just drop them.

---

## New regressions surfaced by v3 enhancements (2)

### G20 — Admin missing import for new target type when `wiring_mode=guice_field_inject`

**Severity:** Breaking — every Guice-mode extract fails compile on the
source class with `cannot find symbol` on the new field's type.

**What happened:** with `wiring_mode=constructor_args` (v1 behavior),
the plan added `import com.sferion.planglobal.backend.service.NonRoutineFlaringAndVentingService;`
to the Admin file. With `wiring_mode=guice_field_inject`, the plan
inserts the `@Inject private TargetService field;` declaration but
does NOT emit the import. Compile error:

```
NonRoutineFlaringAndVentingAdmin.java:57: error: cannot find symbol
    @Inject private NonRoutineFlaringAndVentingService nonRoutineFlaringAndVentingService;
                    ^
  symbol:   class NonRoutineFlaringAndVentingService
```

**Fix shape:** the wiring_mode branch should add the target's
fully-qualified import unconditionally whenever source and target
are in different packages. Same-package wiring needs no import.
The constructor-args path handles this; the field-inject path takes
a different branch and missed it.

### G21 — Service missing import for source class despite v3's auto-qualification rewrites

**Severity:** Breaking — every cluster with cross-class static calls
OR inner-type references back to the source fails compile on the
target.

**What happened:** v3's deep_analysis correctly rewrites cross-class
static calls (`nonRoutineFlaringAndVentingBaseQuery(...)` →
`NonRoutineFlaringAndVentingAdmin.nonRoutineFlaringAndVentingBaseQuery(...)`)
and inner-type method references (`InnerRecord::new` →
`NonRoutineFlaringAndVentingAdmin.NonroutineFlaringVentingDetailsRecord::new`).
But the target file is NOT given an `import` for the source class
that those qualifiers now reference. Compile error:

```
NonRoutineFlaringAndVentingService.java:879: error: package NonRoutineFlaringAndVentingAdmin does not exist
    public @Nullable NonRoutineFlaringAndVentingAdmin.NonroutineFlaringVentingDetailsRecord fetchLatestNonRoutineFlaringAndVentingForAssignedId(
                                                     ^
```

(And the same root cause for the 2 static-call sites and the 1
`::new` site — they all need the source-class import.)

**Fix shape:** when v3's rewriter emits `<SourceClass>.X` in the
generated target (static call qualifier, inner-type qualifier,
method-reference qualifier), the import-collection pass should add
the source class's import unconditionally. Today the rewrites and
the import pass are out of sync — rewriter writes the use-site,
importer doesn't see it.

---

## Hand-fix tally

| Pre-closure (v1 tooling) | Post-closure (v3 tooling) |
|---|---|
| 1. Replace Admin's synthesized null-capturing ctor with `@Inject` field | (closed by G7 fix) |
| 2. Add `@Slf4j` + lombok import to Service | Only lombok import left (G13 partial) |
| 3. Add `@Nullable` import to Service | (closed by G16 fix) |
| 4. Rewrite 4 record `.r`/`.site`/`.plant` → accessor calls | Still 4 sites (G17 unclosed) |
| 5. Qualify 1 `::new` method reference | (closed by G18 fix) |
| 6. Qualify 2 static call sites + drop 2 FIXME comments | (closed by G19 fix) |
| 7. Add 3 delegate wrappers on Admin | (closed by G5 fix — 5 wrappers auto-generated) |
| | NEW: Add target's import to Admin (G20) |
| | NEW: Add source-class import to Service (G21) |
| **7 edits total** | **4 edits total** |

---

## Cosmetic observations (not blockers)

- Admin's field block now reads
  `@Inject private NonRoutineFlaringAndVentingService nonRoutineFlaringAndVentingService;    @Inject\n    private RuntimeProductionAdmin runtimeProductionAdmin;`
  — note the `@Inject` immediately following the `;` on the same line
  without a newline. Parses fine but ugly. Existing formatting bug
  noted in earlier passes; not introduced by v3.
- Service imports are missing blank-line separators between top-level
  / project / java-stdlib / static groups. Compiles, just style.
