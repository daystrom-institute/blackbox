---
title: "bro-harness diagnostics — check & truth tiers (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The deferred upper tiers of window-0 diagnostics: the check tier (flycheck lints with ≥2-check persistence gating + scope honesty) and the orchestrator-owned truth tier (the expensive --all-features workspace-global pass at ownership transfer). Plus the open migrate-vs-copy decision for the bro-lsp session core vs the daemon's src/lsp. The instant/error tier MVP is shipped; these tiers are additive over the existing {class,tier,confidence,scope} envelope."
---

# bro-harness diagnostics — check & truth tiers (backlog)

> **Provenance.** Extracted from [`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md).
> The full rationale for *why* these tiers are shaped this way lives in that
> as-built record; this is the actionable residual. The instant/error tier MVP
> (`crates/bro-lsp` + `diagnostics::{engine,render}` +
> `append_window0_diagnostics`) is **shipped on main**.

## Status / gate

**Deferred — sound design, low value now.** The instant tier already captures the
catastrophically-compounding diagnostics (types, borrows, unresolved names). The
check tier targets lints this very design argues *do not* compound, largely
duplicates the `cargo check`/`clippy` an agent already runs before committing,
and performs worst on the hub crate where it would help most (~8.8–16s vs ~380ms
on a leaf crate). **Revisit trigger:** config-fragile lints become a real pain
point, or crates are split enough that check-tier latency is leaf-crate-fast.

## Work items

- **Check tier (flycheck lints).** `unused_imports`, `dead_code`, clippy. Pulls
  in disproportionate machinery: ≥2-check persistence gating (only surface a lint
  that persisted across checks), the `Scope`/`candidate` config-fragility model,
  and the no-fix-from-fragile re-verification (which exists only to handle a
  hazard the tier itself creates). The `{class,tier,confidence,scope}` envelope
  vocabulary already lives in `diagnostics/mod.rs`, so this is additive.
  **Pre-work:** confirm whether the MVP already surfaces RA flycheck warnings
  crudely through the `publishDiagnostics` drain — if so, the work is gating +
  scope-honesty, not new surfacing.
- **Truth tier (orchestrator-owned).** The expensive workspace-global
  `--all-features` pass. **Not a harness responsibility at all** — owned by
  whoever performs the ownership transfer (the orchestrator at collection, or an
  explicit solo act), because the harness, as the payload being transferred,
  cannot observe the boundary it is the payload of (invariant DX-7). Wire it on
  the transfer boundary, not the edit loop.
- **Open fork decision: `bro-lsp` session core vs `src/lsp`.** The MVP built the
  session core fresh in the shared `bro-lsp` crate and left the daemon's
  `src/lsp/` untouched (DX-9: no dependency on the `blackbox` lib). The
  migrate-vs-copy fork is still open — decide whether to unify the daemon onto
  the shared crate or keep the two session managers separate.
- **(Orthogonal, optional.)** A daemon-side `bbox_code_diagnostics` tool for
  non-harness agents — separate surface, not part of the window-0 edit loop.

## Acceptance

- Check-tier lints surface only after ≥2 checks and only when the crate compiles
  clean of errors, with scope-honest payloads (`candidate`/`deferred` where a
  per-crate rustc cannot answer a workspace-global question).
- Truth-tier runs at ownership transfer, owned by the orchestrator/solo act —
  never gated inside the harness edit loop.
- Fork decision recorded (migrate or copy), `note-b59cadc5` updated.

## Relationship

- Parent / full rationale: [`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md).
- The volatile-injection seam this rides: [`bro-harness-hooks.md`](./bro-harness-hooks.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).
