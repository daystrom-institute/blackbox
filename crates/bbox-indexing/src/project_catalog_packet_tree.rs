//! Project-catalog owner codec for a state directory's packet tree.
//!
//! A state directory may hold a `packets/` tree with one JSON record per file
//! (`<scope>/<id>.json`). No daemon store opens it. The project catalog still
//! treats it as an owner: the migration inventories and stamps its rows, and
//! project retirement probes and discharges them. Only the fields the catalog
//! reads are modelled; every other field survives a stamp untouched.

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

/// The catalog-relevant projection of one packet record.
#[derive(Debug, Deserialize)]
struct PacketRow {
    id: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

/// Capture packet rows that retain the legacy literal project selector.
///
/// A missing packet directory remains missing instead of being created as an
/// empty store.
pub fn capture_project_catalog_owner_snapshot(
    packets_dir: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, OwnerSnapshotStateV1,
        build_owner_snapshot, capture_stable_regular_tree_nofollow, corrupt_owner_snapshot,
        finalize_owner_snapshot, missing_owner_snapshot, owner_subsource, sha256_hex,
        stable_subsource_id,
    };

    match std::fs::symlink_metadata(packets_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("packet", "packet:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => return corrupt_owner_snapshot("packet", "packet:root", "owner_tree_unsafe", limits),
    }
    let captures =
        match capture_stable_regular_tree_nofollow(packets_dir, "packet", limits, |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        }) {
            Ok(captures) => captures,
            Err(error) => {
                return corrupt_owner_snapshot("packet", "packet:root", error.code, limits);
            }
        };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "packet",
            vec![owner_subsource("packet:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("packet", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "packet",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let packet: PacketRow = match serde_json::from_slice(&bytes) {
            Ok(packet) => packet,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "packet",
                    &subsource_id,
                    "owner_source_invalid",
                    limits,
                );
            }
        };
        let mut subsource_rows = Vec::new();
        if let Some(project_id) = packet
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project_id| !project_id.is_empty())
        {
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{}:target", packet.id),
                project_id,
                sha256_hex(&bytes),
            ));
        }
        if packet.project_id.is_none()
            && let Some(project) = packet
                .project
                .map(|project| project.trim().to_string())
                .filter(|project| !project.is_empty())
        {
            subsource_rows.push(OwnerSnapshotRowV1::legacy_selector(
                packet.id,
                LegacyProjectSelectorKindV1::Project,
                project,
            ));
        }
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot("packet", "packet:root", subsources, rows, limits)
}

/// Stamp one packet with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a packet already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    packets_dir: &Path,
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
    use bbox_corpus_core::project_catalog_snapshot::stamp_json_tree_row;

    stamp_json_tree_row(
        packets_dir,
        "packet",
        limits,
        |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        },
        |_subsource_id, document| {
            document
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        },
        source_row_id,
        project_id,
    )
}

/// Read the stable project ids of MANY packet rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`]. Locates the records exactly as the
/// stamper does, so the two agree on row identity by construction.
///
/// Batched over the whole requested set because this owner is a TREE: a per-row
/// caller walks every packet file once per row.
pub fn read_project_catalog_owner_rows(
    packets_dir: &Path,
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
    use bbox_corpus_core::project_catalog_snapshot::read_json_tree_rows_project_id;

    read_json_tree_rows_project_id(
        packets_dir,
        "packet",
        limits,
        |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        },
        |_subsource_id, document| {
            document
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        },
        source_row_ids,
    )
}

/// Remove packet files owned by one project.
/// Missing stores are empty and malformed files refuse retirement.
pub fn discharge_project_catalog_rows(
    packets_dir: &Path,
    project_id: &str,
    selectors: &[String],
) -> Result<usize> {
    let metadata = match fs::symlink_metadata(packets_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("packet store root is not a safe directory");
    }
    let mut removals = Vec::new();
    for scope in ["global", "project"] {
        let directory = packets_dir.join(scope);
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                anyhow::bail!("packet store contains a non-canonical entry");
            }
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".json.lock"))
                {
                    continue;
                }
                anyhow::bail!("packet store contains a non-canonical entry");
            }
            let packet: PacketRow = serde_json::from_slice(&fs::read(entry.path())?)?;
            let owned = match packet.project_id.as_deref() {
                Some(owner) => owner == project_id,
                None => packet
                    .project
                    .as_ref()
                    .is_some_and(|project| selectors.iter().any(|selector| selector == project)),
            };
            if owned {
                removals.push(entry.path());
            }
        }
    }
    for path in &removals {
        fs::remove_file(path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(removals.len())
}

#[cfg(test)]
mod discharge {
    use super::*;

    fn write_packet(root: &Path, scope: &str, id: &str, project_id: &str, project: &str) {
        let directory = root.join(scope);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("{id}.json")),
            serde_json::json!({
                "id": id,
                "domain": format!("demo/{project_id}"),
                "scope": scope,
                "project": project,
                "project_id": project_id,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn retirement_discharge_removes_only_owned_packets_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("packets");
        write_packet(&root, "project", "packet-0000000a", "project-a", "/repo/a");
        write_packet(&root, "project", "packet-0000000b", "project-b", "/repo/a");

        assert_eq!(
            discharge_project_catalog_rows(&root, "project-a", &["/repo/a".into()]).unwrap(),
            1
        );
        assert_eq!(
            discharge_project_catalog_rows(&root, "project-a", &["/repo/a".into()]).unwrap(),
            0
        );
        assert!(!root.join("project/packet-0000000a.json").exists());
        assert!(root.join("project/packet-0000000b.json").exists());
    }

    #[test]
    fn a_missing_tree_discharges_nothing_and_stays_missing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("packets");
        assert_eq!(
            discharge_project_catalog_rows(&root, "project-a", &[]).unwrap(),
            0
        );
        assert!(!root.exists());
    }
}

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OwnerRowStampOutcomeV1,
        OwnerSnapshotLimitsV1,
    };

    const SELECTOR_FIELD: &str = "project";

    struct Fixture {
        root: std::path::PathBuf,
        probe: std::path::PathBuf,
        row_a: String,
        row_b: String,
        path_a: std::path::PathBuf,
        path_b: std::path::PathBuf,
    }

    fn document(id: &str, selector: &str, extra: bool) -> Vec<u8> {
        let future = if extra {
            r#", "future_field": {"kept": true}"#
        } else {
            ""
        };
        format!(
            r#"{{"id": "{id}", "project": "{selector}"{future}}}
"#
        )
        .into_bytes()
    }

    fn write_fixture(dir: &tempfile::TempDir) -> Fixture {
        let root = dir.path().canonicalize().unwrap().join("packets");
        std::fs::create_dir_all(&root).unwrap();
        let path_a = root.join("one.json");
        let path_b = root.join("two.json");
        std::fs::write(&path_a, document("pk1", "/legacy/path/one", true)).unwrap();
        std::fs::write(&path_b, document("pk2", "/legacy/path/two", false)).unwrap();
        Fixture {
            row_a: "pk1".to_string(),
            row_b: "pk2".to_string(),
            probe: root.clone(),
            root,
            path_a,
            path_b,
        }
    }

    fn absent_fixture(dir: &tempfile::TempDir) -> Fixture {
        let root = dir.path().canonicalize().unwrap().join("packets");
        Fixture {
            row_a: "any-row".to_string(),
            row_b: "any-row".to_string(),
            path_a: root.join("one.json"),
            path_b: root.join("two.json"),
            probe: root.clone(),
            root,
        }
    }

    fn path_of(fixture: &Fixture, row: &str) -> std::path::PathBuf {
        if row == fixture.row_a {
            fixture.path_a.clone()
        } else {
            fixture.path_b.clone()
        }
    }

    fn read_bytes(fixture: &Fixture, row: &str) -> Vec<u8> {
        std::fs::read(path_of(fixture, row)).unwrap()
    }

    fn read_row(fixture: &Fixture, row: &str) -> serde_json::Value {
        serde_json::from_slice(&read_bytes(fixture, row)).unwrap()
    }

    fn stamp(
        fixture: &Fixture,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            &fixture.root,
            row,
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        assert_eq!(
            stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&fixture, &fixture.row_a);
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row[SELECTOR_FIELD], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&fixture, &fixture.row_b)
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap();
        let after_first = read_bytes(&fixture, &fixture.row_a);

        assert_eq!(
            stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        // Byte-identical: the second stamp elided the write entirely.
        assert_eq!(read_bytes(&fixture, &fixture.row_a), after_first);
    }

    /// Never a silent overwrite: a row bound to another project refuses.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap();
        let before = read_bytes(&fixture, &fixture.row_a);

        let error = stamp(&fixture, &fixture.row_a, "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&fixture, &fixture.row_a)["project_id"], "a1b2c3d4");
        assert_eq!(read_bytes(&fixture, &fixture.row_a), before);
    }

    /// Absence is a refusal, never a success: a resolution naming a row this
    /// store does not have must not report progress.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        let error = stamp(&fixture, "row-does-not-exist", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create it.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = absent_fixture(&dir);

        assert!(stamp(&fixture, &fixture.row_a, "a1b2c3d4").is_err());
        assert!(!fixture.probe.exists());
    }
}
