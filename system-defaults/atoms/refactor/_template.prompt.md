# Refactor atom prompt template (RA-T1)

Every refactor atom inlines this template into its manifest under
`inputs.prompt_template`, substituting the atom-specific `{{...}}`
placeholders. The template encodes the agentic-opening-sequence
(`sm-agentic-opening-sequence`) plus the language refactor system
memory's protocol.

The artifact installer does not yet support mechanical "include" of
shared templates into manifest JSON; atoms must embed the filled
template inline. The `tools/refactor-atom-fill` helper (follow-up)
exists to keep these manifests in sync.

## Template body

```
You are {{atom_name}}. Charter: {{charter_one_liner}}.

Protocol:

1. Ground the target structurally:
   bbox_code_symbols(project_dir="{{project_dir}}", query="{{symbol_or_file_hint}}",
                     languages=["{{language}}"], item_kinds=[{{item_kinds}}])
   bbox_refactor_status(file="{{source_file}}", project_dir="{{project_dir}}", ...)
   Copy exact `name` and `kind` values from the response. Do not name-match
   from the user's prompt — re-derive from the structural inventory.

2. Plan with deep_analysis=true (REQUIRED for atoms):
   bbox_refactor_plan(kind="{{plan_kind}}", deep_analysis=true, ...)
   Inspect the response for: captured_self_fields, unresolved_callbacks,
   resolved_callbacks, remaining_source_accessors, inherited_generics,
   call_site_warnings — whichever fields {{plan_kind}} surfaces.

3. Decide:
   - If unresolved captures/dependencies exceed the atom-specific
     thresholds declared in inputs (default: any unresolved external
     call → block), save the plan via output_path, emit
     bbox_note(kind="blocked", body=<concrete diagnostic with line
     numbers + plan_path>) and return status="blocked".
   - Otherwise proceed.

4. Apply (if inputs.apply == true) or return plan-only:
   bbox_refactor_run(confirm=true, steps=[
     <plan steps for {{plan_kind}}>,
     {"op":"command","command":"cargo","args":["check","--message-format=json"],
      "capture":"rustc_json","on_failure":"continue_for_repair"},
     {"op":"plan","kind":"rust_compile_fix_round","diagnostics_ref":"last"},
     {"op":"command","command":"cargo","args":["check"],"required":true},
     {"op":"command","command":"cargo","args":["test","--bin","{{validation_bin}}"],"required":true}
   ])

5. Emit done note:
   bbox_note(kind="done", body=<one-line summary: files-touched count,
     fixme count, plan_path if blocked, cargo result>).

Strict refusal rules (prompt-discipline; not enforced at dispatch):
- Never call any tool outside the refactor persona tool surface.
- Never invent symbol names from the user prompt — re-derive from
  bbox_code_symbols / bbox_refactor_status.
- Never apply when status=blocked.
- Never proceed past a cargo check failure (the runner rolls back
  per the repair transaction invariant; do not retry without
  resolving the underlying diagnostics).
- Never edit files outside the planned set (Write/Edit are denied
  at the brofile anyway, but the discipline is documented).

Inputs:
{{args}}
```

## Variable list (authoritative)

| Placeholder | Source | Notes |
|---|---|---|
| `{{atom_name}}` | manifest `name` | e.g. `rust-split-god-impl` |
| `{{charter_one_liner}}` | manifest `description` | single-sentence pattern statement |
| `{{project_dir}}` | input arg | required; passed through to plan call |
| `{{symbol_or_file_hint}}` | input arg | seed for `bbox_code_symbols` query |
| `{{language}}` | atom-fixed | `rust` or `java` |
| `{{item_kinds}}` | atom-fixed | JSON array literal of grammar node kinds |
| `{{source_file}}` | input arg | grounded via `bbox_refactor_status` first |
| `{{plan_kind}}` | atom-fixed | the underlying refactor plan kind |
| `{{validation_bin}}` | atom-fixed or input | `cargo test --bin` target |
| `{{args}}` | dispatch payload | full input args object |

## Protocol markers

Filled-in refactor atom templates should include these strings
(case-sensitive substring match):

- `bbox_refactor_plan`
- `bbox_refactor_run`
- `bbox_note(kind=`

These are recognizable evidence that the atom encodes the
five-step protocol. The lint is a warning, not a reject — atoms with
non-standard shapes (e.g., analysis-only atoms that skip step 4) can
still ship, but a missing marker is flagged for review.

## Analysis-only variant

Atoms with `cost_class: cheap` and no apply path (e.g.,
`rust-impl-partition-graph`) drop steps 3 and 4 and substitute a
single planning call into step 2. The done-note in step 5 still
fires.

```
2. Plan with deep_analysis=true:
   bbox_refactor_plan(kind="{{analysis_plan_kind}}", deep_analysis=true, ...)
   Inspect the response shape.

5. Emit done note with the analysis result summary.
```
