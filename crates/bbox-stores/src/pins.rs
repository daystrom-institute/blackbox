//! Rows of the legacy central pin file (`blackbox-pins.json`).
//!
//! The daemon no longer reads or writes this file at runtime. The project
//! catalog migration still inventories, stamps and verifies its project rows,
//! so this module keeps the file-level readers it needs. Row parsing is
//! deliberately partial: only `id` and `project` are modelled and every
//! other field is ignored, so any historical row shape loads.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LegacyPinStore {
    #[serde(default)]
    pins: Vec<LegacyPinRow>,
}

#[derive(Debug, Deserialize)]
struct LegacyPinRow {
    id: String,
    #[serde(default)]
    project: Option<String>,
}

/// Capture pin rows that retain the legacy literal project selector.
pub fn capture_project_catalog_owner_snapshot(
    store_path: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(store_path, "pin", "pin:central-json", limits, |bytes| {
        let store: LegacyPinStore = serde_json::from_slice(bytes).map_err(|_| ())?;
        Ok(store
            .pins
            .into_iter()
            .filter_map(|pin| {
                let selector = pin.project?.trim().to_string();
                (!selector.is_empty()).then(|| {
                    OwnerSnapshotRowV1::legacy_selector(
                        pin.id,
                        LegacyProjectSelectorKindV1::Project,
                        selector,
                    )
                })
            })
            .collect())
    })
}

/// Stamp one pin row with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a row already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(store_path, "pin", "pin:central-json", limits, |bytes| {
        stamp_json_array_row(bytes, "pins", "id", source_row_id, project_id)
    })
}

/// Read the stable project ids of MANY central pin rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
/// Batched over the whole requested set, so verifying this owner costs ONE
/// locked capture and answers every row from ONE durable snapshot.
pub fn read_project_catalog_owner_rows(
    store_path: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(store_path, "pin", "pin:central-json", limits, |bytes| {
        read_json_array_rows_project_id(bytes, "pins", "id", source_row_ids)
    })
}

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
        OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    };

    /// Two pins plus a field this binary does not model, so every test also
    /// witnesses preservation of data the compiled schema cannot see.
    fn write_fixture(store_path: &Path) {
        std::fs::write(
            store_path,
            br#"{
  "version": 1,
  "pins": [
    {
      "id": "pin-0001",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "pin-0002",
      "project": "/legacy/path/two"
    }
  ]
}
"#,
        )
        .unwrap();
    }

    fn stamp(
        store_path: &Path,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            store_path,
            row,
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
        document["pins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row)
            .cloned()
            .unwrap()
    }

    fn fixture_store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_path = dir.path().canonicalize().unwrap().join("pins.json");
        write_fixture(&store_path);
        store_path
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        assert_eq!(
            stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_path, "pin-0001");
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row["project"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&store_path, "pin-0002")
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap();
        let after_first = std::fs::read(&store_path).unwrap();

        assert_eq!(
            stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        // Byte-identical: the second stamp elided the write entirely.
        assert_eq!(std::fs::read(&store_path).unwrap(), after_first);
    }

    /// Never a silent overwrite: a row bound to another project refuses.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap();
        let before = std::fs::read(&store_path).unwrap();

        let error = stamp(&store_path, "pin-0001", "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&store_path, "pin-0001")["project_id"], "a1b2c3d4");
        assert_eq!(std::fs::read(&store_path).unwrap(), before);
    }

    /// Absence is a refusal, never a success: a resolution naming a row this
    /// store does not have must not report progress.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        let error = stamp(&store_path, "row-does-not-exist", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create a store.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().canonicalize().unwrap().join("pins.json");

        let error = stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_path.exists());
    }

    /// A complete historical pin row (every field the retired store wrote)
    /// still captures; rows without a project selector are skipped.
    #[test]
    fn capture_reads_legacy_rows_and_ignores_unmodelled_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().canonicalize().unwrap().join("pins.json");
        std::fs::write(
            &store_path,
            br#"{
  "version": 1,
  "pins": [
    {
      "id": "pin-0001",
      "title": "Active arc",
      "content": "Phase two only.",
      "scope": "work_item",
      "target": "wi-1",
      "project": "/legacy/path/one",
      "project_id": "a1b2c3d4",
      "expires_at": "2026-01-01T00:00:00Z",
      "created_at": "2025-12-01T00:00:00Z",
      "updated_at": "2025-12-01T00:00:00Z"
    },
    {
      "id": "pin-0002",
      "title": "Unscoped",
      "content": "No project.",
      "scope": "session",
      "target": "sess-1",
      "created_at": "2025-12-01T00:00:00Z",
      "updated_at": "2025-12-01T00:00:00Z"
    }
  ]
}
"#,
        )
        .unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&store_path, OwnerSnapshotLimitsV1::default())
                .unwrap();
        let ids: Vec<&str> = snapshot
            .rows
            .iter()
            .map(|row| row.stable_row_id.as_str())
            .collect();
        assert_eq!(ids, vec!["pin-0001"]);
        assert!(format!("{:?}", snapshot.rows[0].value).contains("/legacy/path/one"));
    }
}
