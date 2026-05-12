# Rust Refactor — v2 invariants and deferred plan kinds

Status: proposed (deferred items from `refactor-rust-expansion-impl` non-goals).
Related: `design/archive/refactor-rust-expansion.md`,
`design/archive/refactor-rust-expansion-impl.md` ("Non-goals (this skeleton)"
and "Cross-surface invariants" sections),
`design/proposed/refactor-agents-impl.md` (consumer that exercises these
invariants via atom dispatch — the agents skeleton is the natural pull
for v2 enforcement).

## Thesis

The Rust refactor expansion landed 25 RX-* phases. The skeleton was
explicit about what it *didn't* ship:

- v1 of cross-surface invariants is **documentation + audit fields**.
  Runtime enforcement was deferred to v2.
- A handful of plan kinds and runner features were tagged "future" or
  "open question" without a home.

This doc collects those items so they can be picked up when the
refactor-agents pull (the consumer that actually exercises atom
dispatch + command allowlist) lands.

## Items

### V2 runtime enforcement of operator-authority opt-outs (RX-V1)

Source: `archive/refactor-rust-expansion-impl.md` RX-V1.

v1 ships `operator_opt_outs_used` as an audit field on durable
`RefactorPlan`. v2: dispatch-side check that an agent's invocation
passes `acknowledge_repr` / `acknowledge_public_api_change` flags only
from declared `inputs`, never as a constant. Requires per-dispatch
tool-call provenance (out of scope for the RX skeleton; the dispatch
surface in `refactor-agents-impl` is the home).

### V2 runtime enforcement of atom command-allowlist (RX-V2)

Source: `archive/refactor-rust-expansion-impl.md` RX-V2.

v1 ships the cargo-only command allowlist as docs + atom-prompt-template
encoding. v2 adds runner-side enforcement: runner accepts
`dispatch_origin: "agent" | "operator"` flag set by `bro_agent_dispatch`;
when `agent`, runner enforces the allowlist server-side.

Pairs with V1 — both gate on dispatch-origin metadata that the
agents-impl skeleton is the natural producer of.

### RX-C1 multi-round compile-fix loop semantics

Source: `archive/refactor-rust-expansion-impl.md` non-goals.

Open question deferred from the RX-C1 ship: under what termination
condition does a compile-fix round loop? Current v1 ships single-round
repair driven by `continue_for_repair` + obligations. Multi-round is
viable but the termination criterion (fixed-point on rustc diagnostics?
budget on iterations? operator-acknowledged "give up"?) was left open
deliberately.

### Cargo-semver-checks integration (RX-G2)

Source: `archive/refactor-rust-expansion-impl.md` non-goals.

RX-G2 ships `rust_public_api_guard` as a structural advisory.
Integrate `cargo-semver-checks` as the authoritative source for
public-API change detection, replacing or augmenting the structural
check. Future.

### `dejunk_rust_struct` plan kind

Source: `archive/refactor-rust-expansion-impl.md` non-goals
("`dejunk_rust_struct` modernization plan kind — separate doc").

Modernization sweep over a Rust struct: drop unused fields, collapse
`Option<()>` to `bool`, lift trivial getters, etc. Java has
`lombokify_java_class` as the parallel. Needs its own design round —
the Java analog is heavy and the Rust equivalent is not obvious
(no Lombok-style attribute macros in scope).

### Out-of-scope (kept here for traceability)

These were explicit non-goals in the impl skeleton and are not
intended to migrate. Listed so future readers don't try to revive
them inside RX:

- **Macro-expansion-aware analysis.** Out per design.
- **Workspace-wide multi-crate type indexing.** Project-local only.
- **Auto-detection of `driver_share_groups` in RX-P1.** Operator
  passes explicit groups in v1; auto-detection deferred indefinitely.

## Picking the next one

V1+V2 are the high-value pair — they unlock safe agent dispatch of
refactor atoms, which is the entire point of `refactor-agents`. If
the agents skeleton lands, V1+V2 should land with it.

The remaining items (C1 loop semantics, semver-checks, dejunk) are
opportunistic.
