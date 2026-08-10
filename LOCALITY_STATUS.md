# Locality Program Status

Updated: 2026-08-10

## Current production state

- The macOS production daemon remains disabled.
- The cage daemon is running the verified `f06c114cd749` image at immutable digest `sha256:74087b6fd6eec72b1b56fe7d5d68dfd3b06370f75ea5100b84172e07f85e9d2d` with zero restarts. Pod: `blackboxd-dd45fddd5-rx927`.
- No maintenance pod currently has the production PVC mounted.
- The final collector refresh completed successfully for all 19 published projects. Code, Git history, provenance, and published knowledge all reached durable terminal states.
- The two stale provenance journals were explicitly revalidated against their current code selectors and converged to `Committed` with their exact prior edge counts.
- The strict Git transport marker is applied and verified for all 19 repositories. The final clean preflight had 18 exact history matches, one accepted external checkout proof, zero stale selectors, and zero prepared journals. Marker checksum: `bd0627d72243c723462a539d3a6cfb5ad2346282d2b11b387af7018ee17aa581`.
- The current deployed startup rebuilt the complete 506,002-document EdgeIndex from 54 sidecars in 15.4 seconds. It loaded 1,159,878,981 sidecar bytes and 35,565 code blobs with zero degraded publications.
- Worker dispatch now reaches the daemon ClusterIP through the checkout host's accepted cage subnet route. The HA tailnet Service VIP currently resolves but has no approved backend and times out; it remains unavailable for operator and collector HTTPS until its service advertisements are approved by tailnet policy.
- Authenticated checkout-local `bro provenance export` completed 210 pages for generation `28e9b7a85b7eb2b57084f2bc4b047c9a60e54940051b1efe3e52c844f171c4b1`: 671 existing note documents were unchanged, 0 were rejected, and no stale-generation restart occurred. The export caused a real sidecar rebuild while the daemon continued serving; that rebuild converged in 10.3 seconds and the graph remained intact.
- The current catalog has 19 projects and all 19 are Published. The blame locality marker is applied and offline-verified for the full set at catalog epoch 49. Marker checksum: `f3ed9e9e95e3448cccda2b7d90ab4599d319ad70b07ac683410a31a83ecaf422`.
- After the marked daemon restart, corpus-entity `bbox_blame` refuses with `error.blame_locality_required` before any checkout acquisition. Authenticated checkout-local `bro blame` succeeds against a real file and line. The checkout observation sequence remains exactly 327234; only the expected operator completion advanced the blame observation sequence from 169 to 170. The pod remains ready with zero restarts.
- Managed project render is fully live-accepted for five disposable Published checkouts. Each has successful all-provider, non-dry-run `published`, `all`, and `own` receipts with three writes and zero refusals. Two additional projects have successful zero-write completions because they contain no project guidance. A large managed checkout has successful three-write `published` and `all` receipts; its `own` view exposed an expired-provisional sequence defect described below. Durable render observation sequence is 23. The complete daemon checkout-observation file remains byte-for-byte unchanged at sequence 327234 with no `render_file_provider` counter, and the pod remains ready with zero restarts.

## Landed implementation

- `f004ca1c3688`: added canonical external checkout-parity proof acceptance and bound it to the live catalog, code selectors, history generation, and vector/document commitments.
- `1c3d334b503a`: changed the code-source locality marker from a permanently pinned evidence generation to stable producer/scope authority plus a healthy current collected generation.
- `74d5e27a4cd2`: repaired committed provenance-journal revalidation after a code-selector advance.
- `e0c260bd15bd`: added authenticated checkout-local provenance planning and made every incomplete graph view fail closed behind an explicit readiness fence.
- `035f42e49b0f`: moved fleetd provider-binary eligibility to the executor host instead of testing the containerized daemon host.
- `e450bdd5c477`: preserved the interactive MCP surface for direct cockpit dispatches while keeping workflow and recursive automation on their restricted surfaces.
- `78b994d923dc`: added typed MCP host-authority configuration, the `BBOX_MCP_ALLOWED_HOSTS` override, strict validation, and rmcp server wiring.
- `cbcdd2748070`: repaired standalone harness locality configuration lookup across task-local and process environment forms.
- `391eb8c6d419`: admitted the daemon-authored source endpoint only through the managed harness's explicit encrypted-network policy and made fatal harness startup errors independent of inherited log filters.
- `f06c114cd749`: paged managed render plans below the MCP response cap, pinned the compact canonical plan by SHA-256, and made the checkout-owner harness validate every page before completing the plan.
- `916029e6ec86`: repaired the provisional probe so it returns the next durable workspace sequence under the store mutation lock, forbade new uploads from reusing finalized sequences or installed generation identities, made finalize journals monotonic in stage and fixed in identity, and added startup recovery for the legacy duplicate-finalize journal shape. Focused gates passed locally (30/30 knowledge-source crate tests, 7/7 daemon handler tests, clippy, pinned fmt). Full cluster verification `bbox-verify-md8vt` passed nextest, clippy, and concurrency; image build and deployment have not yet occurred.
- Every deployed implementation commit passed its full cluster verification gate. Exact rerun `bbox-verify-crlcw` passed all 6,436 tests plus clippy and concurrency for `f06c114cd749`; image build `build-bbox-image-x8mzx` produced the current immutable cage digest.
- The live Pulumi deployment is pinned to the current exact image digest. Cage commit `34eee75` records and publishes the matching worker routing and image pin.

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

1. Build the image for the verified repair `916029e6ec86`, redeploy the exact repaired daemon and checkout-owner harness, then rerun all three views for the large managed checkout and prove the checkout-observation file is unchanged. Startup recovery must also reconstruct the committed journal clobbered by the duplicate upload.
2. Migrate the eleven hand-authored provider-guidance sets and give the two zero-content projects a provider-neutral source document, then complete three-view coverage for all 19 projects. The operator approved absorbing the hand-authored guidance into provider-neutral source documentation on 2026-08-10.
3. Run the render locality preflight, quiet window, offline marker apply, restart, and strict refusal/successor probes.
4. Inventory the separately scoped raw blame and bridge compatibility categories after render coverage is strict.

## Next-lane inventory

- Production now has complete three-view render-locality acceptance for five Published projects and still has no render locality marker or `RenderFileProvider` target counters. Two no-content projects have valid zero-write completions. One large project has two of three successful views. The other eleven projects still need provider-guidance migration and complete three-view evidence.
- The 19 bound checkout roots are mapped and none of their four instruction targets is currently dirty. Seven already have all three blackbox-managed provider files; one has all three provider files absent. The other eleven contain hand-authored provider guidance that render must preserve rather than overwrite: eleven `CLAUDE.md` files and nine `AGENTS.md` files. `GEMINI.md` is managed in seven checkouts and absent in twelve. Only six checkouts currently have a nonempty `PROJECT.md`.
- Five of the eight initially writable checkouts have completed the full three-view managed proof. Two lack any source guidance and need a provider-neutral source document before a three-write proof is possible. The large checkout is blocked only on the sequence defect below. The other eleven need deliberate bootstrap/decomposition first, and every remaining checkout needs a managed plan proving that each view produces three written projections rather than skipped output.
- Historical checkout counters show the already-governed Git history, collected source, publisher, knowledge, artifact-watch, and blame paths refusing their daemon checkout lanes. The remaining explicit work is project render evidence plus the separately scoped bridge/raw compatibility categories; global render remains host-local by design.

## Managed-render acceptance

- The large managed checkout proved that `f06c114cd749` fixes render plans above the generic MCP response cap: its `published` and `all` calls each assembled the paged plan and wrote all three provider projections.
- Its `own` call then failed because the prior provisional lease had expired. The client inferred sequence one from an absent live pointer even though immutable sequence one history remained. A second exact upload reached `Prepared`, collided with the retired immutable generation, and replaced the prior committed journal.
- The repair landed as `916029e6ec86`: the probe returns the next durable sequence under the store mutation lock, new uploads cannot reuse finalized sequences, journals enforce monotonic stage and fixed identity, and startup recovery reconstructs the committed journal only when all immutable evidence and the original Ready upload agree. After verification and deployment, the large checkout must rerun all three views and advance durable render observations by exactly three while checkout observations remain unchanged.

- The first managed render dispatch exposed a separate executor-boundary regression: the allocator rejected every fleetd lane as `provider_binary_missing` because it resolved `bro-harness` on the containerized daemon host, even though fleetd owns the worker process and its login-shell `PATH`.
- Commit `035f42e49b0f` makes provider-binary eligibility executor-local. Local execution retains the daemon-host binary gate, fleetd execution defers final resolution to the worker host, and the non-dispatchable workflow pseudo-provider still fails closed. Full local and cluster verification passed, image build `build-bbox-image-cb45d` succeeded, and digest `sha256:025872bdd9db0b877a0744620d411b499600db8a63ebc094fdcb5495bba08fdd` is deployed with zero restarts. Live allocator preview reports `executor_host` and admits a pinned remote lane without a daemon-host harness binary.
- The first admitted Brodex probe reached its worker but stopped on the provider usage limit before a model turn. A GLM retry then exposed the next integration defect: cockpit dispatch rewrote the MCP URL to `surface=default`, whose server-side policy explicitly removes `bbox_render`. The harness therefore could not install its managed-workspace locality wrapper and made no render calls or file changes.
- Commit `e450bdd5c477` passed the exact cluster gate, was built as immutable digest `sha256:83b8e1032a59ed0e7605209ee4c3fec4e54a92f721040699105056ab69257dff`, and is live with zero restarts. Cockpit workers now receive `surface=interactive`; workflow workers remain on `agent-internal`, and recursive agent/atom dispatches remain on `default`.
- The first post-deploy GLM probe proved the surface URL was correct but made zero render calls. Its cwd was an ordinary base checkout, so fleetd correctly classified it as unmanaged and the harness received no workspace binding header. Remote workspace authority requires an independent clone carrying the exact managed-checkout marker plus committed matching project identity; ordinary base checkouts and linked worktrees do not satisfy that contract.
- A disposable managed clone at the exact selected Published revision now proves workspace inspection and token injection: the live harness receives both `surface=interactive` and the secret workspace-binding header. Its first MCP transport used the tailnet Service VIP and timed out because neither HA ingress replica had an approved service advertisement.
- Cage now derives a worker-only MCP URL from the stable daemon ClusterIP and routes it over the accepted encrypted cage subnet. The direct path returns health 200, but exact MCP initialize exposed one more daemon defect: `BBOX_MCP_ALLOWED_HOSTS` was present in the deployment yet ignored by blackboxd, so rmcp returned `403 Forbidden: Host header is not allowed` and the harness again loaded zero tools.
- Commit `78b994d923dc` repaired the host-authority defect, passed full cluster verification, and is live by immutable digest. Direct MCP initialize with the exact daemon ClusterIP authority now returns HTTP 200 and a valid interactive session without a workspace token.
- The first managed render dispatch after that repair received a valid workspace binding, loaded `mcp__blackbox__bbox_render`, and reached the daemon. Its first `published` call failed closed with `error.render_locality_required`; `all` and `own` were deliberately not attempted after the first failure. No provider file was changed.
- The next dispatch exposed a standalone harness locality-installation defect. The daemon injected the binding, source URL, and published scope into the child process environment, but `install_project_mutation_routes` read only `ToolCx.session_env`, whose standalone task-local snapshot is empty by design. The raw MCP tool therefore bypassed `LocalRenderTool` even though daemon authentication succeeded.
- The resulting repair resolves those three daemon-authored values through the established task-local-then-process environment seam and adds a regression proving tool-context precedence plus standalone fallback.
- Commit `cbcdd2748070` contains that standalone environment repair, passed the full cluster gate, and was installed as the stablesigned arm64 host harness with its prior binary preserved. The matching linux image build also succeeded but was not deployed because this change belongs to the worker harness, not the cage daemon.
- The first repaired startup then exposed a transport-policy mismatch before any model event: the generic knowledge-source client rejected the daemon-authored ClusterIP HTTP endpoint even though the remote fleet contract requires that route to sit behind an encrypted ACL boundary and the same routed endpoint already carries MCP successfully. The three injected locality values were present with valid shapes; no credential value was logged.
- The transport repair keeps the generic client strict, adds an explicit trusted-daemon endpoint constructor for the managed harness only, keeps redirects disabled and the origin pinned, and makes fatal harness startup errors visible even when fleetd's inherited `RUST_LOG` filter excludes the harness target.
- Commit `391eb8c6d419` passed all 6,433 full-profile tests plus clippy and concurrency, then was installed as the stablesigned arm64 worker harness. The cage daemon was not restarted because the repaired code belongs to the standalone worker process.
- Live task `27780b75-e9ca-467c-bf90-6e9df7aa2f59` completed normally. Its three render calls wrote all three fixed provider files for `published`, `all`, and `own`; durable observation sequence 3 contains one all-provider, non-dry-run, 3-written, 0-refused completion per view.
- The rendered provider files share SHA-256 `28147e04c5e8bf22182e6f7e4aa590b7b8e93a4f5780d736bfb2531429ab7e94`. The before and after checkout-observation snapshots are byte-identical at SHA-256 `74a57f37e3d5ccbb2bf11614912e7de78c4fc1e8bb91a55492c2746620ee8c8c`; sequence stayed 327234 and no render checkout counter appeared.
- All three views surfaced the same bounded diagnostic that legacy compatibility rows lack a provable `built_from` stamp. This did not create a refusal, widen checkout authority, or prevent exact receipt validation. The next program step is the remaining-project rollout, including deliberate migration of the eleven hand-authored provider-doc sets before any strict marker ceremony.
- Gap `gap-194793e9` is addressed after the live successor proof; its resolution records the successful three-view checkout-owner render and unchanged daemon checkout observations.

## Preserved operator state

- Do not terminate Claude processes.
- Do not re-enable the macOS production daemon.
- Keep the repository-local build target absent; builds use the external target directory.
- Preserve the operator's untracked project knowledge entry and the host-global `BLACKBOX.md` contents.
