# examples/agentic-corpus — producer machinery shipped with the daemon

Files in this tree are reusable artifacts that drive the agentic-corpus
producer-side workflows: rule-packets, brofiles, workflow specs, cron
routing, and install scripts. They're checked in alongside the daemon
binary so the bbox crate can ship sane defaults without depending on
host-local config.

Cross-link from the [main README](../../README.md) and the
[operating notes](../../docs/operating-blackbox.md).

## Layout

```
examples/agentic-corpus/
  brofiles/      — persona+model+lens triples for actors
  crons/         — cron schedules that drive periodic arcs
  packets/       — rule-packets (deterministic decision functions)
  scripts/       — install-team-from-brofiles, etc
  workflows/     — JSON workflow specs (agentic dispatch graphs)
```

## Workflows (`workflows/`)

JSON specs the workflow engine compiles + dispatches. Install via
`bro_workflow_install` or load via `bbox_artifact_install`. Run with
`bro_orchestrate_run`.

| File | Purpose |
|---|---|
| `project-bootstrap-arc.json` | Skeleton arc fired by `bbox_project_register`. **Currently a stub** — each node sets a bool and goes to next. Real chunking is wired into the auto-reindex thread (this stub may be replaced once the bootstrap-arc gap closes; see `thread-3cfbf9e0`). |
| `schema-migration-arc.json` | Drains compaction targets after a schema bump |
| `embed-compaction-arc.json` | Re-embed + WAL compaction sweep — pairs with `crons/embed-compaction-nightly.json` |
| `nightly-eval-arc.json` | Skeleton for an eval harness gate; relies on the deferred `op: shell` engine primitive — see `thread-3cfbf9e0` |
| `auto-digest-arc.json` | Distill a session into knowledge candidates via the `digest-extractor` brofile, gate via `domain:auto-digest/entry-quality` packet |
| `auto-edge-arc.json` | Propose new knowledge↔knowledge / knowledge↔file edges via the auto-edge brofiles + `domain:auto-edge/vote-aggregate` packet |
| `contradiction-review-arc.json` | Tier-0 cosine-similarity-detected contradiction review — opens a whiteboard, spins three specialist brofiles, synthesizes a verdict |

Each workflow carries a `policy_packet` reference for arc-budget +
retry-ceiling enforcement (see `packets/workflow-policy/`).

## Rule-packets (`packets/`)

Deterministic decision functions, evaluated by `bbox_apply` (no LLM in
the loop). Each domain is a directory of versioned packet JSONs +
`audit/` cases for fidelity validation.

| Domain | Packet | Used by |
|---|---|---|
| `auto-digest/` | `entry-quality.json` | auto-digest-arc QualityGate node |
| `auto-edge/` | `vote-aggregate.json` | auto-edge-arc Aggregate node |
| `bro-trust/` | `bro-trust.json` | gates whether to auto-apply or hold-for-review based on the originating brofile's trust class |
| `contradiction/` | `review-synthesis.json` | contradiction-review-arc ApplyVerdict node |
| `cron-routing/` | `task-completed-routing.json` | routes `task-completed` signals to the auto-digest arc (gated on the engine emitting that signal — currently deferred) |
| `embed/` | `compaction-trigger.json` | nightly compaction trigger gate |
| `eval/` | `parity-strictness.json` | eval-suite per-class checker |
| `workflow-policy/` | `arc-budget.json` | universal arc-step + retry budget |

Install all packets via `bbox_artifact_install kind=packet` per file, or
use `bbox_compile` to rebuild from the bundled examples.

## Brofiles (`brofiles/`)

Persona+model+lens triples installed via `bro_brofile create` or as F4
catalog artifacts. The lens is a system prompt stub that loads on
dispatch; brofiles also carry a `tools` allow/disallow filter so a
"read-only" brofile can't accidentally edit files.

Grouped by arc:

- **Auto-digest**: `digest-extractor` — extracts knowledge candidates
  from session transcripts.
- **Auto-edge**: `describe-narrative-cohesion`, `describe-prose-signal`,
  `describe-symbol-fit` (DESCRIBES vote panel) +
  `reference-citation-precision`, `reference-context-fit`,
  `reference-target-existence` (REFERENCES vote panel).
- **Contradiction-review**: `contradiction-coherence`,
  `contradiction-lifecycle`, `contradiction-provenance` (specialist
  panel) + `contradiction-facilitator` (synthesizer).

## Crons (`crons/`)

Cron-shaped JSON specs the daemon's cron registry installs via
`bro_cron_install`. Each spec maps a cron expression to a workflow id
and a vars payload.

| File | Schedule | Drives |
|---|---|---|
| `embed-compaction-nightly.json` | `0 3 * * *` (3am daily) | `embed-compaction-arc` |

## Scripts (`scripts/`)

| File | Purpose |
|---|---|
| `install-teams.sh` | One-shot installer that creates the contradiction-specialists + auto-edge-classifiers ensemble teams from the brofiles already installed. Run after the brofile catalog has been seeded. |

## Lifecycle: how the artifacts get installed

The daemon does NOT auto-install everything in this tree on startup
(that would mutate operator state without consent). Instead:

1. **First-time setup**: operator picks the artifacts they want and
   runs `bbox_artifact_install` (or the per-kind tool — `bro_workflow_install`,
   `bro_brofile create`, etc) for each. The F4 catalog tracks what was
   installed from where with version + supersession metadata.

2. **Updates**: `bbox_artifact_supersede` retires an old version and
   activates a new one. The daemon doesn't watch this dir for changes;
   you have to actively re-install.

3. **Inspection**: `bbox_artifact_list` shows what's currently installed
   (kind / name / version / source path / superseded_by).

## Status / known gaps

These artifacts are functional but several arcs depend on engine
primitives that are still deferred:

- `auto-digest-arc` → needs the `task-completed` signal which `bro_exec`
  doesn't emit yet (tracked in `thread-3cfbf9e0`).
- `auto-edge-arc` → processes ONE candidate per invocation; full batch
  should be rewritten onto native workflow fanout (tracked in
  `thread-cba8bfa1`).
- `nightly-eval-arc` → workflow gates can't consume subprocess stdout
  yet; arc records that the harness ran but can't route on actual drift
  (tracked in `thread-3cfbf9e0`).
- `contradiction-review-arc` → the team
  `contradiction-specialists` must exist before dispatch
  (`scripts/install-teams.sh`).

Pull the deeper runbook for any of these via `bbox_knowledge(query="sm-...")`:

- `sm-rule-packets` — packet authoring, evaluation, audit
- `sm-workflow-orchestration` — workflow spec authoring
- `sm-bro-dispatch-patterns` — exec / resume / wait usage
- `sm-whiteboards` — multi-agent deliberation surfaces
