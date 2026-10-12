# Warm lanes and cluster gates

Use this file only on an estate that provides the operator-local `bbox-cage`
tooling. Its own `build/README.md` and
`build/lanes/BBOX_LANE_WORK.md` define the active resource contract; read them
before heavy work. Do not copy private infrastructure details into this repo.

Claim an existing warm pool lane before creating or rebuilding one:

```sh
~/repos/bbox-cage/build/lanes/lane-pool.sh claim
```

Record its name and printed checkout path. Use that path as the dispatch `cwd`,
and tell workers to read the lane contract before running build commands. Wait
for the contract's warmth marker rather than compiling into a half-provisioned
lane. Preserve its cache configuration and local managed-checkout marker.
Cargo, rustc, and sccache route into the builder pod according to the lane.

For verification of an already-pushed ref:

```sh
~/repos/bbox-cage/build/submit-bbox-verify.sh --ref <ref> --watch
```

Read the workflow's actual phase and step results. A successful submit/watch
process exit is not the verification verdict. On failure, use
`~/repos/bbox-cage/build/verify-triage.sh <workflow-name>` before composing ad hoc
log searches. Compare failure evidence against the exact ref and lane result.
A controlled same-ref retry can test reproducibility, but repeated failure
alone does not prove an infrastructure fault. Inspect the failing test and base
state before choosing a repair.

For a requested Linux image build:

```sh
~/repos/bbox-cage/build/submit-bbox-build.sh --ref <ref> --watch
```

Preserve the resulting image identity and receipt. Image construction is not
deployment; macOS satellites need their separate native build/install path.

Lane release destroys the lane and requests a refill. Before release, ensure
workers have finished and authorized durable changes are saved elsewhere. If
push is forbidden, arrange an authorized patch/ref transfer before destroying
the only copy. Release only the lane this task owns:

```sh
~/repos/bbox-cage/build/lanes/lane-pool.sh release <name>
```

If the pool is broken or a base looks stale, inspect status and the external
runbook. Surface a cheap operator-side remedy before starting a long rebuild.
Do not recycle, destroy, or repair a lane owned by another task.
