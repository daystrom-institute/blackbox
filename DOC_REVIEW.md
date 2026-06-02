# Design Document Review

Dispatch a heterogeneous 5-lens review ensemble against a design document
and get a structured review verdict in ~10 minutes.

## Quick Start

Point Claude at a design document:

> "read DOC_REVIEW.md and run it against design/my-proposal.md"

Claude will read this file, install artifacts if needed, dispatch the
workflow, poll for completion, and present the review.

## What Happens

The `blackbox-review` workflow opens a whiteboard, broadcasts the design
document to a 5-member review panel plus an independent validator, runs a
bridgecrew-aligned deliberation (blind → validate → debate → resolve), and
writes a review document to `design/review/<slug>-review.md`.

The five review lenses:

| Lens | Dimension | Owns the question |
|------|-----------|-------------------|
| Soundness | SND | Does the design correctly solve the stated problem? |
| Precision | PRC | Are claims precisely specified with honest evidence grades? |
| Economy | ECO | Is scope bounded, decomposable, and proportional? |
| Resilience | RES | What breaks during/after implementation? |
| Corroboration | COR | Does history/context confirm the signal? |

Each lens runs on a different provider (Claude, Brodex, DeepSeek, GLM) for
genuine perspective diversity. An independent validator audits every finding
with evidence-backed confirmed/refuted/inconclusive verdicts. Refuted findings
are excluded from the review. Unresolved contradictions survive for human
judgment — the system never force-resolves disagreement.

## Prerequisites

### Artifacts must be installed

Check with `bbox_artifact_list` or ask Claude to run the install sequence:

```
# Brofiles (7: 5 lenses + validator + facilitator)
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-snd.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-prc.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-eco.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-res.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-cor.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-validator.json")
bbox_artifact_install(kind="brofile", source=".bbox/brofiles/blackbox-review-facilitator.json")

# Gate packet
bbox_artifact_install(kind="packet", source=".bbox/packets/whiteboard-participation.json")

# Panel teamplate + save
bbox_artifact_install(kind="team", source=".bbox/teams/blackbox-review-panel.json")
bro_team(action="save_template", name="blackbox-review-panel", members=[
  {brofile: "blackbox-review-snd", alias: "soundness", count: 1},
  {brofile: "blackbox-review-prc", alias: "precision", count: 1},
  {brofile: "blackbox-review-eco", alias: "economy", count: 1},
  {brofile: "blackbox-review-res", alias: "resilience", count: 1},
  {brofile: "blackbox-review-cor", alias: "corroboration", count: 1},
])

# Workflow
bbox_artifact_install(kind="workflow", source=".bbox/workflows/blackbox-review.json")

# CRITICAL: instantiate the team (save_template alone is not enough)
bro_team(action="create", name="blackbox-review-panel", template="blackbox-review-panel")
```

### Project must be registered

```
bbox_project_register(path="/path/to/repo")
```

## Dispatch

### Via MCP (from Claude or another agent)

```python
# Read the design document
doc_text = Read("design/my-proposal.md")

# Dispatch the workflow
bro_orchestrate_run(
  workflow_id="blackbox-review",
  project_dir="/path/to/repo",
  max_steps=50,
  await_completion=False,
  initial_vars={
    "project_dir": "/path/to/repo",
    "design_doc_path": "design/my-proposal.md",
    "design_doc_text": doc_text,
    "operator_hints": [
      "pay attention to async runtime boundaries and public API surface"
    ]
  }
)
```

### Via HTTP

```bash
PORT="${BBOX_PORT:-7264}"
PROJECT="/path/to/repo"
DOC_PATH="design/my-proposal.md"
DOC_TEXT="$(cat "$DOC_PATH")"

jq -n \
  --arg project_dir "$PROJECT" \
  --arg design_doc_path "$DOC_PATH" \
  --arg design_doc_text "$DOC_TEXT" \
  '{
    workflow_id: "blackbox-review",
    project_dir: $project_dir,
    max_steps: 50,
    await_completion: false,
    initial_vars: {
      project_dir: $project_dir,
      design_doc_path: $design_doc_path,
      design_doc_text: $design_doc_text,
      operator_hints: ["review this design document against the codebase"]
    }
  }' |
curl -sS -H 'content-type: application/json' \
  -d @- "http://127.0.0.1:${PORT}/orchestrate/by-id" | jq .
```

## Monitor

The dispatch returns `{taskId, arcId}`. Poll with:

```bash
# CLI
bro orchestrate status <arcId>
bro_status(task_id="<taskId>")
bro_arc_status(arc_id="<arcId>")

# Or wait for completion
bro_wait(task_id="<taskId>", timeout_seconds=600)
```

Via MCP from Claude: use `bro_status`, `bro_arc_status`, or `bro_wait` with
the returned IDs.

## Output

On success, the workflow writes:

```
<project>/design/review/<design-doc-stem>-review.md
```

The review document contains:

- **YAML frontmatter**: `kind: design-review`, `lifecycle: proposed`,
  `generated_by: blackbox-review-ensemble`
- **Summary**: overall assessment
- **Findings by Dimension**: surviving findings organized by SND/PRC/ECO/RES/COR
- **Contradictions Requiring Human Judgment** (if any): unresolved disagreements
  between lenses — genuine signal, not failure
- **Refuted Claims** (if any): validator-excluded findings with evidence
- **Verdict**: one of `approve` / `approve-with-concerns` / `revise` / `reject`
- **Actionable Next Steps**

Review the document, address contradictions, tighten acceptance criteria if
needed, then act on the verdict.

## Reference

- **Workflow orchestration**: `bbox_knowledge(query="sm-workflow-orchestration")`
- **Whiteboard API**: `bbox_knowledge(query="sm-whiteboards")`
- **Ensemble generator** (tailoring for other repos):
  `system-defaults/ENSEMBLE_GENERATOR.md`
- **Generic design-doc exemplar**: `system-defaults/workflows/review/design-doc-review.json`
- **Pathology ensemble** (the original 5-dimension projection):
  `design/refactor-tools/pathology-ensemble-review.md`
- **Pathology dispatch guide**: `docs/pathology-dispatch.md`
