# Locality Program Status

Updated: 2026-08-10

## Current production state

- The macOS production daemon remains disabled.
- The cage daemon is running the verified `74d5e27a` image with zero restarts after the repair collector refresh and strict-marker restart.
- No maintenance pod currently has the production PVC mounted.
- The final collector refresh completed successfully for all 19 published projects. Code, Git history, provenance, and published knowledge all reached durable terminal states.
- The two stale provenance journals were explicitly revalidated against their current code selectors and converged to `Committed` with their exact prior edge counts.
- The strict Git transport marker is applied and verified for all 19 repositories. The final clean preflight had 18 exact history matches, one accepted external checkout proof, zero stale selectors, and zero prepared journals. Marker checksum: `bd0627d72243c723462a539d3a6cfb5ad2346282d2b11b387af7018ee17aa581`.
- Live BM25 search, entity inspection, a real `THREAD_SPAWNED_FROM` graph traversal, and authenticated checkout-local `bro blame` all succeed against the restarted daemon. The pod remains at zero restarts.
- The first graph read triggered the intentionally deferred edge-index rebuild. While that rebuild was still publishing, the first entity inspection returned no edges; after the rebuild converged, the same entity exposed `NEXT_SECTION`, `CONTAINS_SYMBOL`, and `DEFINED_IN`, and a one-hop path query succeeded. This is a live first-read correctness hazard to disposition, not a completed graph validation.

## Landed implementation

- `f004ca1c3688`: added canonical external checkout-parity proof acceptance and bound it to the live catalog, code selectors, history generation, and vector/document commitments.
- `1c3d334b503a`: changed the code-source locality marker from a permanently pinned evidence generation to stable producer/scope authority plus a healthy current collected generation.
- `74d5e27a4cd2`: repaired committed provenance-journal revalidation after a code-selector advance.
- All three commits passed their full cluster verification gates. The `74d5e27a4cd2` Linux image is deployed in the cage.

## Current repair

The remaining refusal exposed a provenance lifecycle defect. Re-finalizing an unchanged immutable provenance generation did not replace an already committed journal after the active code selector advanced. The collector could therefore finish successfully while the strict cutover correctly continued to refuse stale selector evidence.

Commit `74d5e27a` is pushed and:

- revalidates and republishes an explicitly re-finalized committed provenance import when its pinned selector is no longer current;
- permits the same active immutable import to re-enter `Importing` for that authenticated revalidation;
- documents the lifecycle contract; and
- includes a regression proving the same import generation moves from the old selector to the current selector and returns to `Active`.

Focused provenance-import nextest result: 6 passed, 0 failed. `cargo check --workspace` passes.

Full cluster verification `bbox-verify-d6d7h` passed all 6,418 tests plus clippy and concurrency gates. Image build `build-bbox-image-wmjh4` succeeded with digest `sha256:d96ac0215c54e854250a033d19f07ed2992d3030f2b1c2ffc1327b4dd5ae4f2d`.

The current uncommitted repair set addresses both live validation defects:

- `bro provenance export --token-file` resolves the checkout's committed published scope and authenticates an attended, read-only MCP planning session with the existing scope-bound producer credential. Raw `?project=` remains filter-only authority.
- Deferred EdgeIndex startup and every later selector-changing publication are now explicit fail-closed warmups. The readiness fence lowers before an intentionally empty replacement view is published, the watcher starts or retries the complete rebuild, and graph-dependent tools return `error.edge_index_warming` until the matching complete view lands.
- Focused verification passes 10/10 cases covering exact-scope authentication, successful operator-authorized planning, real MCP initialize wiring, warmup refusal, selector-republish fencing, schema behavior, HTTP provenance export, and startup rebuild selection. The local default workspace run passed 6,416/6,416 tests before the final coherence regressions were added; the final focused set and workspace compile pass on the complete tree. Provenance dependency acceptance, concurrency lint, pinned formatting, and diff hygiene also pass.

## Next operations

1. Commit and push the authenticated provenance and graph-coherence repairs.
2. Run the full cluster gate, build and deploy the image, and repeat live provenance export, warmup, search, graph, path, blame, and restart-stability probes.
3. Generate, apply, and verify the production blame locality marker against a fresh quiet-window preflight.
4. Record the durable compatibility-thread result, then inventory render and the remaining bridge/raw compatibility lanes.

## Preserved operator state

- Do not terminate Claude processes.
- Do not re-enable the macOS production daemon.
- Keep the repository-local build target absent; builds use the external target directory.
- Preserve the operator's untracked project knowledge entry and the host-global `BLACKBOX.md` contents.
