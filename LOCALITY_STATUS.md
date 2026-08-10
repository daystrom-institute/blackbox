# Locality Program Status

Updated: 2026-08-10

## Current production state

- The macOS production daemon remains disabled.
- The cage daemon is temporarily running on the image built from `1c3d334b503a`. It reached Ready with zero restarts after the final collector refresh.
- No maintenance pod currently has the production PVC mounted.
- The final collector refresh completed successfully for all 19 published projects. Code, Git history, provenance, and published knowledge all reached durable terminal states.
- The strict Git transport marker has not been applied yet. The production preflight found 19 repositories, zero prepared journals, 17 proposed repositories, and 2 refusals caused by provenance journals pinned to superseded code selectors. One of those repositories also has the expected checkout/P3 history mismatch that the external parity-proof lane covers.

## Landed implementation

- `f004ca1c3688`: added canonical external checkout-parity proof acceptance and bound it to the live catalog, code selectors, history generation, and vector/document commitments.
- `1c3d334b503a`: changed the code-source locality marker from a permanently pinned evidence generation to stable producer/scope authority plus a healthy current collected generation.
- Both commits passed the full cluster verification gate. The `1c3d334b503a` Linux image is deployed in the cage.

## Active fix

The remaining refusal exposed a provenance lifecycle defect. Re-finalizing an unchanged immutable provenance generation did not replace an already committed journal after the active code selector advanced. The collector could therefore finish successfully while the strict cutover correctly continued to refuse stale selector evidence.

The working tree now:

- revalidates and republishes an explicitly re-finalized committed provenance import when its pinned selector is no longer current;
- permits the same active immutable import to re-enter `Importing` for that authenticated revalidation;
- documents the lifecycle contract; and
- includes a regression proving the same import generation moves from the old selector to the current selector and returns to `Active`.

Focused provenance-import nextest result: 6 passed, 0 failed. `cargo check --workspace` passes.

## Next operations

1. Run the relevant focused suite and compile checks, then commit and push the provenance repair.
2. Run the full cluster verification gate and build the Linux image.
3. Deploy the image, run the collector once, and confirm both stale provenance journals rebind to the current selectors without daemon instability.
4. Take the daemon offline, generate a fresh strict Git preflight, produce and accept the final checkout-parity proof, then apply and verify the Git transport marker against those exact artifacts.
5. Restart the daemon and validate search, graph, blame/provenance, restart stability, and absence of checkout-backed Git/provenance activity.
6. Reconcile the infrastructure state, record the durable compatibility-thread result, then continue with blame, render, and remaining bridge/raw compatibility retirement.

## Preserved operator state

- Do not terminate Claude processes.
- Do not re-enable the macOS production daemon.
- Keep the repository-local build target absent; builds use the external target directory.
- Preserve the operator's untracked project knowledge entry and the host-global `BLACKBOX.md` contents.
