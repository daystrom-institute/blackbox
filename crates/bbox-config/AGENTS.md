# bbox-config: config loader, env override allowlist, producer grants

## `[source_connectors]` grants

- One `connector_source_id` binds one producer and one profile (`file`,
  `conversation`, `api_dataset`); a kind or profile mismatch on an existing
  id refuses instead of forking identity. A dataset source that wants to
  place bytes (`project_owned` placement) uses a separately minted
  file-profile scope; catalog uniqueness is NOT relaxed to `(id, profile)`
  pairs (operator ruling 2026-08-16, api-dataset-connector.md item 1).
- `token_file` and `token_files` are mutually exclusive; `token_files` is
  the overlap-tolerant rotation form (index 0 is the primary, later entries
  are still-accepted predecessors). An `api_dataset` grant that declares
  actions MUST use `token_files`: declared actions with a single
  `token_file` refuse at config validation, not in a rotation runbook. A
  refused action result post during a single-token cutover would strand
  work the vendor already performed (operator ruling 2026-08-16,
  api-dataset-connector.md item 6 / section 9.1).
- Grant token bytes are load-to-validate-then-drop where the lane does not
  need retention, and redacted-Debug where it does; a test asserting the
  Debug output guards the no-credential-leak property.
