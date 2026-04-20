# Render lifecycle — render, absorb, review, lint

The render path is easy to misuse because the verbs are adjacent in the UI but operate on different parts of the lifecycle.

This is the compact model:

- `bbox_render` publishes approved knowledge into managed files.
- `bbox_absorb` imports external edits back into the knowledge store as unverified entries.
- `bbox_review` accepts or rejects those unverified entries.
- `bbox_lint` checks the store for contradictions, duplication, and stale structure.
- `bbox_pin` is not part of this lifecycle. Pins stay out of rendered memory entirely.

## Normal forward path

1. create or update entries with `bbox_learn` / `bbox_decide` / `bbox_remember`
2. `bbox_render`
3. agents consume the managed output

This is the default path when the source of truth is the knowledge store.

## Reverse path after manual edits

1. user edits rendered memory files directly
2. `bbox_absorb`
3. `bbox_review`
4. `bbox_render`

This is the reconcile loop when the rendered files were edited outside the store.

## Scope thinking

### Global render

Use when the guidance should land in the provider-level managed files and affect every project/session on the host.

### Project render

Use when the guidance belongs only to the current repo and its project-local memory files.

## What each verb is not

- `bbox_render` is not a review step. It publishes what is already approved/renderable.
- `bbox_render` is not a hot-context mechanism. If the goal is "keep this active-arc guidance visible across turns for one execution lane," use `bbox_pin`, not render.
- `bbox_absorb` is not publication. It imports external edits into the store, usually as unverified state.
- `bbox_review` is not rendering. It changes whether absorbed entries are accepted.
- `bbox_lint` is not a sync step. It is hygiene/diagnostics.

## When to reach for lint

Use `bbox_lint`:

- before large knowledge-store refactors
- after bulk absorb/review work
- when the rendered output looks inconsistent with what you expected

## Keep hot vs cold

Keep hot in tool docs:

- render publishes
- absorb imports
- review approves
- lint diagnoses

Keep cold here:

- forward vs reverse lifecycle
- scope thinking
- the absorb -> review -> render reconciliation loop
