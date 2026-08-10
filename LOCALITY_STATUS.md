# Locality Program Status

Updated: 2026-08-10

## Current production state

- The macOS production daemon remains disabled.
- The cage daemon is running the verified `78b994d923dc` image at immutable digest `sha256:8f21ce69ddb8ff67458ba7764c83fc6cf4ec1938acecccb2c3c607e8f0270f7a` with zero restarts. Pod: `blackboxd-848ccdc78f-llccb`.
- No maintenance pod currently has the production PVC mounted.
- The final collector refresh completed successfully for all 19 published projects. Code, Git history, provenance, and published knowledge all reached durable terminal states.
- The two stale provenance journals were explicitly revalidated against their current code selectors and converged to `Committed` with their exact prior edge counts.
- The strict Git transport marker is applied and verified for all 19 repositories. The final clean preflight had 18 exact history matches, one accepted external checkout proof, zero stale selectors, and zero prepared journals. Marker checksum: `bd0627d72243c723462a539d3a6cfb5ad2346282d2b11b387af7018ee17aa581`.
- The current deployed startup rebuilt the complete 506,002-document EdgeIndex from 54 sidecars in 15.4 seconds. It loaded 1,159,878,981 sidecar bytes and 35,565 code blobs with zero degraded publications.
- Worker dispatch now reaches the daemon ClusterIP through the checkout host's accepted cage subnet route. The HA tailnet Service VIP currently resolves but has no approved backend and times out; it remains unavailable for operator and collector HTTPS until its service advertisements are approved by tailnet policy.
- Authenticated checkout-local `bro provenance export` completed 210 pages for generation `28e9b7a85b7eb2b57084f2bc4b047c9a60e54940051b1efe3e52c844f171c4b1`: 671 existing note documents were unchanged, 0 were rejected, and no stale-generation restart occurred. The export caused a real sidecar rebuild while the daemon continued serving; that rebuild converged in 10.3 seconds and the graph remained intact.
- The current catalog has 19 projects and all 19 are Published. The blame locality marker is applied and offline-verified for the full set at catalog epoch 49. Marker checksum: `f3ed9e9e95e3448cccda2b7d90ab4599d319ad70b07ac683410a31a83ecaf422`.
- After the marked daemon restart, corpus-entity `bbox_blame` refuses with `error.blame_locality_required` before any checkout acquisition. Authenticated checkout-local `bro blame` succeeds against a real file and line. The checkout observation sequence remains exactly 327234; only the expected operator completion advanced the blame observation sequence from 169 to 170. The pod remains ready with zero restarts.

## Landed implementation

- `f004ca1c3688`: added canonical external checkout-parity proof acceptance and bound it to the live catalog, code selectors, history generation, and vector/document commitments.
- `1c3d334b503a`: changed the code-source locality marker from a permanently pinned evidence generation to stable producer/scope authority plus a healthy current collected generation.
- `74d5e27a4cd2`: repaired committed provenance-journal revalidation after a code-selector advance.
- `e0c260bd15bd`: added authenticated checkout-local provenance planning and made every incomplete graph view fail closed behind an explicit readiness fence.
- `035f42e49b0f`: moved fleetd provider-binary eligibility to the executor host instead of testing the containerized daemon host.
- `e450bdd5c477`: preserved the interactive MCP surface for direct cockpit dispatches while keeping workflow and recursive automation on their restricted surfaces.
- `78b994d923dc`: added typed MCP host-authority configuration, the `BBOX_MCP_ALLOWED_HOSTS` override, strict validation, and rmcp server wiring.
- Every deployed implementation commit passed its full cluster verification gate. Image build `build-bbox-image-lmkmn` produced the current immutable cage digest.
- The live Pulumi deployment is pinned to the current exact image digest. The matching cage source changes remain uncommitted until managed-render acceptance succeeds.

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

## In-flight managed-render acceptance

- The first managed render dispatch exposed a separate executor-boundary regression: the allocator rejected every fleetd lane as `provider_binary_missing` because it resolved `bro-harness` on the containerized daemon host, even though fleetd owns the worker process and its login-shell `PATH`.
- Commit `035f42e49b0f` makes provider-binary eligibility executor-local. Local execution retains the daemon-host binary gate, fleetd execution defers final resolution to the worker host, and the non-dispatchable workflow pseudo-provider still fails closed. Full local and cluster verification passed, image build `build-bbox-image-cb45d` succeeded, and digest `sha256:025872bdd9db0b877a0744620d411b499600db8a63ebc094fdcb5495bba08fdd` is deployed with zero restarts. Live allocator preview reports `executor_host` and admits a pinned remote lane without a daemon-host harness binary.
- The first admitted Brodex probe reached its worker but stopped on the provider usage limit before a model turn. A GLM retry then exposed the next integration defect: cockpit dispatch rewrote the MCP URL to `surface=default`, whose server-side policy explicitly removes `bbox_render`. The harness therefore could not install its managed-workspace locality wrapper and made no render calls or file changes.
- Commit `e450bdd5c477` passed the exact cluster gate, was built as immutable digest `sha256:83b8e1032a59ed0e7605209ee4c3fec4e54a92f721040699105056ab69257dff`, and is live with zero restarts. Cockpit workers now receive `surface=interactive`; workflow workers remain on `agent-internal`, and recursive agent/atom dispatches remain on `default`.
- The first post-deploy GLM probe proved the surface URL was correct but made zero render calls. Its cwd was an ordinary base checkout, so fleetd correctly classified it as unmanaged and the harness received no workspace binding header. Remote workspace authority requires an independent clone carrying the exact managed-checkout marker plus committed matching project identity; ordinary base checkouts and linked worktrees do not satisfy that contract.
- A disposable managed clone at the exact selected Published revision now proves workspace inspection and token injection: the live harness receives both `surface=interactive` and the secret workspace-binding header. Its first MCP transport used the tailnet Service VIP and timed out because neither HA ingress replica had an approved service advertisement.
- Cage now derives a worker-only MCP URL from the stable daemon ClusterIP and routes it over the accepted encrypted cage subnet. The direct path returns health 200, but exact MCP initialize exposed one more daemon defect: `BBOX_MCP_ALLOWED_HOSTS` was present in the deployment yet ignored by blackboxd, so rmcp returned `403 Forbidden: Host header is not allowed` and the harness again loaded zero tools.
- Commit `78b994d923dc` repaired the host-authority defect, passed full cluster verification, and is live by immutable digest. Direct MCP initialize with the exact daemon ClusterIP authority now returns HTTP 200 and a valid interactive session without a workspace token.
- The first managed render dispatch after that repair received a valid workspace binding, loaded `mcp__blackbox__bbox_render`, and reached the daemon. Its first `published` call failed closed with `error.render_locality_required`; `all` and `own` were deliberately not attempted after the first failure. No provider file was changed.
- The current defect is in standalone harness locality installation. The daemon injects the binding, source URL, and published scope into the child process environment, but `install_project_mutation_routes` reads only `ToolCx.session_env`, whose standalone task-local snapshot is empty by design. The raw MCP tool therefore bypasses `LocalRenderTool` even though daemon authentication succeeds.
- The in-flight repair resolves those three daemon-authored values through the established task-local-then-process environment seam and adds a regression proving tool-context precedence plus standalone fallback. Acceptance remains incomplete until the exact repair passes verification, is deployed, and all three views produce local writes and durable path-free receipts with unchanged daemon checkout counters.
- Commit `cbcdd2748070` contains that standalone environment repair, passed the full cluster gate, and was installed as the stablesigned arm64 host harness with its prior binary preserved. The matching linux image build also succeeded but was not deployed because this change belongs to the worker harness, not the cage daemon.
- The first repaired startup then exposed a transport-policy mismatch before any model event: the generic knowledge-source client rejected the daemon-authored ClusterIP HTTP endpoint even though the remote fleet contract requires that route to sit behind an encrypted ACL boundary and the same routed endpoint already carries MCP successfully. The three injected locality values were present with valid shapes; no credential value was logged.
- The current repair keeps the generic client strict, adds an explicit trusted-daemon endpoint constructor for the managed harness only, keeps redirects disabled and the origin pinned, and makes fatal harness startup errors visible even when fleetd's inherited `RUST_LOG` filter excludes the harness target.

## Preserved operator state

- Do not terminate Claude processes.
- Do not re-enable the macOS production daemon.
- Keep the repository-local build target absent; builds use the external target directory.
- Preserve the operator's untracked project knowledge entry and the host-global `BLACKBOX.md` contents.
