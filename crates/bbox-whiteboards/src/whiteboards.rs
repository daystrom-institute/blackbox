//! Historical whiteboard record shapes and the project-catalog owner adapters
//! that capture and stamp them. Nothing loads or serves boards at runtime.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ── Phase + roles ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Blind,
    Read,
    Validate,
    Debate,
    Resolve,
    Archived,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blind => "blind",
            Self::Read => "read",
            Self::Validate => "validate",
            Self::Debate => "debate",
            Self::Resolve => "resolve",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Specialist,
    Facilitator,
    Operator,
}

// ── Posts / annotations / votes ────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostType {
    Proposal,
    Claim,
    Concern,
    Informational,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: String,
    pub agent: String,
    #[serde(rename = "type")]
    pub post_type: PostType,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cascade_targets: Vec<String>,
    pub posted_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationType {
    Challenge,
    Corroborate,
    Resolve,
    Validation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationResult {
    Confirmed,
    Refuted,
    Inconclusive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub post_id: String,
    pub agent: String,
    #[serde(rename = "type")]
    pub annotation_type: AnnotationType,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ValidationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<String>,
    pub posted_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VoteValue {
    Accept,
    Reject,
    Defer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub post_id: String,
    pub agent: String,
    pub vote: VoteValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: String,
}

// ── Agent registration ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub role: Role,
    /// Free-form domain hint ("security", "perf", "design", …).
    /// Workflow-level concept; the engine doesn't enforce semantics.
    pub domain: String,
    pub registered_at: String,
}

// ── Phase history ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseEvent {
    pub phase: Phase,
    pub by: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

// ── Board ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    /// Original deliberation topic.
    pub topic: String,
    pub project: String,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub created_at: String,
    pub phase: Phase,
    pub phase_history: Vec<PhaseEvent>,
    pub agents: BTreeMap<String, Agent>,
    pub posts: Vec<Post>,
    pub annotations: Vec<Annotation>,
    pub votes: Vec<Vote>,
    /// Original workflow thread identity, retained as historical provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_thread_id: Option<String>,
}

/// Capture persisted boards that retain a legacy literal project selector.
/// This does not create the store directory.
pub fn capture_project_catalog_owner_snapshot(
    storage_dir: &std::path::Path,
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

    match std::fs::symlink_metadata(storage_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("whiteboard", "whiteboard:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "whiteboard",
                "whiteboard:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures =
        match capture_stable_regular_tree_nofollow(storage_dir, "whiteboard", limits, |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        }) {
            Ok(captures) => captures,
            Err(error) => {
                return corrupt_owner_snapshot("whiteboard", "whiteboard:root", error.code, limits);
            }
        };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "whiteboard",
            vec![owner_subsource("whiteboard:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("whiteboard", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "whiteboard",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let board: Board = match serde_json::from_slice(&bytes) {
            Ok(board) => board,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "whiteboard",
                    &subsource_id,
                    "owner_source_invalid",
                    limits,
                );
            }
        };
        let mut subsource_rows = Vec::new();
        if let Some(project_id) = board
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project_id| !project_id.is_empty())
        {
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{}:target", board.id),
                project_id,
                sha256_hex(&bytes),
            ));
        }
        let selector = board.project.trim().to_string();
        if board.project_id.is_none() && !selector.is_empty() {
            subsource_rows.push(OwnerSnapshotRowV1::legacy_selector(
                board.id,
                LegacyProjectSelectorKindV1::Project,
                selector,
            ));
        }
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot("whiteboard", "whiteboard:root", subsources, rows, limits)
}

/// Stamp one board with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a board already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    storage_dir: &std::path::Path,
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
        storage_dir,
        "whiteboard",
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

/// Read the stable project ids of MANY whiteboard rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`]. Locates the records exactly as the
/// stamper does, so the two agree on row identity by construction.
///
/// Batched over the whole requested set because this owner is a TREE: a per-row
/// caller walks every board file once per row.
pub fn read_project_catalog_owner_rows(
    storage_dir: &std::path::Path,
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
        storage_dir,
        "whiteboard",
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

// ── Project-catalog row stamping (P6-B) ─────────────────────────

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
        let root = dir.path().canonicalize().unwrap().join("whiteboards");
        std::fs::create_dir_all(&root).unwrap();
        let path_a = root.join("one.json");
        let path_b = root.join("two.json");
        std::fs::write(&path_a, document("wb1", "/legacy/path/one", true)).unwrap();
        std::fs::write(&path_b, document("wb2", "/legacy/path/two", false)).unwrap();
        Fixture {
            row_a: "wb1".to_string(),
            row_b: "wb2".to_string(),
            probe: root.clone(),
            root,
            path_a,
            path_b,
        }
    }

    fn absent_fixture(dir: &tempfile::TempDir) -> Fixture {
        let root = dir.path().canonicalize().unwrap().join("whiteboards");
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
