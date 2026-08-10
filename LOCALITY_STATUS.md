# Locality Program Status

Updated: 2026-08-10

## Current production state

- The macOS production daemon remains disabled.
- The cage daemon is running the verified `e0c260bd15bd` image at immutable digest `sha256:6bebcb2d63f43d0a055911a347385bf956178271681eeb19aa21583296fc1dfd` with zero restarts.
- No maintenance pod currently has the production PVC mounted.
- The final collector refresh completed successfully for all 19 published projects. Code, Git history, provenance, and published knowledge all reached durable terminal states.
- The two stale provenance journals were explicitly revalidated against their current code selectors and converged to `Committed` with their exact prior edge counts.
- The strict Git transport marker is applied and verified for all 19 repositories. The final clean preflight had 18 exact history matches, one accepted external checkout proof, zero stale selectors, and zero prepared journals. Marker checksum: `bd0627d72243c723462a539d3a6cfb5ad2346282d2b11b387af7018ee17aa581`.
- The deployed startup rebuilt the 505,881-document EdgeIndex from 54 sidecars in 15.4 seconds. The post-marker restart rebuilt the same complete view in 22.4 seconds. Live BM25 search, entity inspection with the expected `THREAD_SPAWNED_FROM` edge, and the corresponding one-hop path all succeed after convergence.
- Authenticated checkout-local `bro provenance export` completed 210 pages for generation `28e9b7a85b7eb2b57084f2bc4b047c9a60e54940051b1efe3e52c844f171c4b1`: 671 existing note documents were unchanged, 0 were rejected, and no stale-generation restart occurred. The export caused a real sidecar rebuild while the daemon continued serving; that rebuild converged in 10.3 seconds and the graph remained intact.
- The current catalog has 19 projects and all 19 are Published. The blame locality marker is applied and offline-verified for the full set at catalog epoch 49. Marker checksum: `f3ed9e9e95e3448cccda2b7d90ab4599d319ad70b07ac683410a31a83ecaf422`.
- After the marked daemon restart, corpus-entity `bbox_blame` refuses with `error.blame_locality_required` before any checkout acquisition. Authenticated checkout-local `bro blame` succeeds against a real file and line. The checkout observation sequence remains exactly 327234; only the expected operator completion advanced the blame observation sequence from 169 to 170. The pod remains ready with zero restarts.

## Landed implementation

- `f004ca1c3688`: added canonical external checkout-parity proof acceptance and bound it to the live catalog, code selectors, history generation, and vector/document commitments.
- `1c3d334b503a`: changed the code-source locality marker from a permanently pinned evidence generation to stable producer/scope authority plus a healthy current collected generation.
- `74d5e27a4cd2`: repaired committed provenance-journal revalidation after a code-selector advance.
- `e0c260bd15bd`: added authenticated checkout-local provenance planning and made every incomplete graph view fail closed behind an explicit readiness fence.
- All four commits passed their full cluster verification gates. Image build `build-bbox-image-7kcq5` succeeded and the exact `e0c260bd15bd` image is deployed in the cage.
- Cage deployment commit `c4a805e` pins that exact image digest.

## Verified repair

The remaining refusal exposed a provenance lifecycle defect. Re-finalizing an unchanged immutable provenance generation did not replace an already committed journal after the active code selector advanced. The collector could therefore finish successfully while the strict cutover correctly continued to refuse stale selector evidence.

Commit `74d5e27a` is pushed and:

- revalidates and republishes an explicitly re-finalized committed provenance import when its pinned selector is no longer current;
- permits the same active immutable import to re-enter `Importing` for that authenticated revalidation;
- documents the lifecycle contract; and
- includes a regression proving the same import generation moves from the old selector to the current selector and returns to `Active`.

Focused provenance-import nextest result: 6 passed, 0 failed. `cargo check --workspace` passes.

Full cluster verification `bbox-verify-d6d7h` passed all 6,418 tests plus clippy and concurrency gates. Image build `build-bbox-image-wmjh4` succeeded with digest `sha256:d96ac0215c54e854250a033d19f07ed2992d3030f2b1c2ffc1327b4dd5ae4f2d`.

Commit `e0c260bd15bd` is pushed and addresses both live validation defects:

- `bro provenance export --token-file` resolves the checkout's committed published scope and authenticates an attended, read-only MCP planning session with the existing scope-bound producer credential. Raw `?project=` remains filter-only authority.
- Deferred EdgeIndex startup and every later selector-changing publication are now explicit fail-closed warmups. The readiness fence lowers before an intentionally empty replacement view is published, the watcher starts or retries the complete rebuild, and graph-dependent tools return `error.edge_index_warming` until the matching complete view lands.
- Focused verification passes 10/10 cases covering exact-scope authentication, successful operator-authorized planning, real MCP initialize wiring, warmup refusal, selector-republish fencing, schema behavior, HTTP provenance export, and startup rebuild selection. The local default workspace run passed 6,416/6,416 tests before the final coherence regressions were added; the final focused set and workspace compile pass on the complete tree. Provenance dependency acceptance, concurrency lint, pinned formatting, and diff hygiene also pass. Full cluster verification `bbox-verify-m7xr7` passed nextest, clippy, and concurrency for the exact commit. Image build `build-bbox-image-7kcq5` succeeded with digest `sha256:6bebcb2d63f43d0a055911a347385bf956178271681eeb19aa21583296fc1dfd` and that image is live.

## Next operations

1. Migrate the hand-authored provider guidance into provider-neutral project documentation and reviewed knowledge without losing content.
2. Generate successful all-provider `published`, `all`, and `own` render completions without daemon checkout access.
3. Run the render locality preflight, quiet window, offline marker apply, restart, and strict refusal/successor probes.
4. Inventory the separately scoped raw blame and bridge compatibility categories after render coverage is strict.

## Next-lane inventory

- Production has no `render-locality-observations.json`, no render locality marker, and no `RenderFileProvider` target counters. The render cutover is therefore not preflight-ready: each selected project first needs successful managed all-provider writes for the `published`, `all`, and `own` views.
- The 19 bound checkout roots are mapped and none of their four instruction targets is currently dirty. Seven already have all three blackbox-managed provider files; one has all three provider files absent. The other eleven contain hand-authored provider guidance that render must preserve rather than overwrite: eleven `CLAUDE.md` files and nine `AGENTS.md` files. `GEMINI.md` is managed in seven checkouts and absent in twelve. Only six checkouts currently have a nonempty `PROJECT.md`.
- Eight checkouts can accept provider writes immediately. The other eleven need deliberate bootstrap/decomposition first, and every checkout still needs a managed plan proving that each view produces three written projections rather than skipped output.
- Historical checkout counters show the already-governed Git history, collected source, publisher, knowledge, artifact-watch, and blame paths refusing their daemon checkout lanes. The remaining explicit work is project render evidence plus the separately scoped bridge/raw compatibility categories; global render remains host-local by design.

## In-flight allocator repair

- The first managed render dispatch exposed a separate executor-boundary regression: the allocator rejected every fleetd lane as `provider_binary_missing` because it resolved `bro-harness` on the containerized daemon host, even though fleetd owns the worker process and its login-shell `PATH`.
- The current checkout makes provider-binary eligibility executor-local. Local execution retains the daemon-host binary gate, fleetd execution defers final resolution to the worker host, and the non-dispatchable workflow pseudo-provider still fails closed.
- Three focused regression tests, `cargo check --workspace`, pinned formatting, concurrency lint, and the fleetd dependency acceptance check pass. The full default workspace nextest gate passed all 6,421 tests with zero failures. This repair is not considered live until the exact commit passes cluster verification, is built and deployed by immutable digest, and a bounded managed dispatch succeeds while the daemon stays up.

## Preserved operator state

- Do not terminate Claude processes.
- Do not re-enable the macOS production daemon.
- Keep the repository-local build target absent; builds use the external target directory.
- Preserve the operator's untracked project knowledge entry and the host-global `BLACKBOX.md` contents.
