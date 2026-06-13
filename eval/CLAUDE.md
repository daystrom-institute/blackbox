# eval — search/agentic eval suite

- **`expected_entity_refs` are content-addressed and rot by design**:
  symbol `defn_hash` and project_file `chunk_hash`/occurrence_idx shift
  with any content change, file move, or chunker reflow. Run
  `eval/scripts/refresh_expected_refs.sh` (report) / `--apply` (rewrite)
  before trusting ANY ranking metric — 24/30 manifests were stale by
  2026-06 and zeroed every sweep. Manifests are `include_str!` into
  `eval/check.rs` (`MANIFEST_SOURCES`), so a new query file must be added
  there and parse failures break the build, not the run.
- **Refresh derives against the BASE checkout, writes to the CURRENT
  checkout.** Refs embed the registered base's project_id and the daemon
  indexes base content, so `project_dir` must be the base; but a worktree
  run must never mutate the shared base's files (this split exists because
  the first version wrote 15 manifests into the base mid-session).
- `eval/scripts/rerank_cap_sweep.py` is the gap-39b3ce16 protocol driver
  (metrics mirror `bbox_corpus_core::search::metrics`). Validated 1.75:
  MRR saturates exactly there; caps ≥1.6875 never bind (max boost
  product). Re-sweep after any ranking change, never hand-tune.
- Transcript-span refs (`transcript:<provider>:<session>:<byte>:<idx>`)
  cannot be auto-refreshed — byte offsets shift on reparse; the script
  reports them for manual relocation.
