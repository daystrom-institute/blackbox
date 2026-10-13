# Warm lanes and cluster gates

Use this file only on an estate that provides the operator-local `bbox-cage`
tooling. Two documents define the lane contract:
`build/lanes/BBOX_LANE_WORK.md` carries the Blackbox details and names the
shared lane contract it extends. Read both before heavy work. `build/README.md`
covers the cluster builds. Do not copy private infrastructure details into
this repo.

Claim an existing warm pool lane before creating or rebuilding one:

```sh
~/repos/bbox-cage/build/lanes/lane-pool.sh claim [--ref <branch>] [--wait <seconds>]
```

A claim hands out only a lane that has finished warming and prints its
checkout path; the lane name is the directory above the checkout. `--ref`
checks the lane out on a pushed branch, and `--wait` waits for a lane that is
still warming instead of failing. A claim that fails after it took a lane
names that lane in its error; release it rather than leaving it claimed. Use
the printed path as the dispatch `cwd`, and give workers the absolute paths of
both contract documents to read before they run build commands. Preserve the lane's cache configuration and local
managed-checkout marker. Cargo, rustc, and sccache route into the builder pod
according to the lane.

When the pool cannot serve the work, create a lane by name from the estate
root (`bin/estate lane create <name> --family bbox`). A created lane is not
warm until the contract's warmth marker appears in its provisioning log. Wait
for the marker rather than compiling into a half-provisioned lane.

To run a gate against a pushed ref in a lane you hold:

```sh
~/repos/bbox-cage/build/lanes/lane-run.sh <name> --ref <sha or origin/branch> -- <command>
```

It fetches, resets the lane checkout to that ref, refusing a checkout with
tracked modifications, and runs the command there, so bare cargo names route
into the pod. Name the handed-over commit by its SHA or as `origin/<branch>`:
a bare branch name resolves to the lane's local branch, which the fetch does
not move. Confirm the commit it reports before trusting the result. This is
how a dispatcher gates a worker's handed-over ref.

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

Lane release destroys the lane and requests a refill. Do not hand a used lane
back to the pool. Before release, ensure workers have finished and
authorized durable changes are saved elsewhere. If push is forbidden, arrange
an authorized patch/ref transfer before destroying the only copy. Release only
the lane this task owns:

```sh
~/repos/bbox-cage/build/lanes/lane-pool.sh release <name>
```

A lane created by name is destroyed from the estate root with
`bin/estate lane destroy <name> --yes`.

If the pool is broken or a base looks stale, inspect
`~/repos/bbox-cage/build/lanes/lane-pool.sh status` and the lane contract's
remedies. Surface a cheap operator-side remedy before starting a long rebuild.
Do not recycle, destroy, or repair a lane owned by another task.
