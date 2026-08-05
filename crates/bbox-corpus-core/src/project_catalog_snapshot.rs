//! Read-only owner snapshots used by the durable project-catalog migration.
//!
//! These values intentionally do not implement `Serialize`. Legacy selector
//! literals are host-local migration evidence and must not accidentally enter
//! a persisted report. Owner crates decode their own durable schemas and use
//! this module only for the common bounded snapshot and commitment contract.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerSnapshotLimitsV1 {
    pub max_source_bytes: usize,
    pub max_subsources: usize,
    pub max_rows: usize,
    pub max_selector_bytes: usize,
}

impl Default for OwnerSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024 * 1024,
            max_subsources: 100_000,
            max_rows: 100_000,
            max_selector_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerSnapshotStateV1 {
    Present {
        content_sha256: String,
        byte_len: u64,
    },
    Missing {
        fingerprint: String,
    },
    Corrupt {
        diagnostic_code: String,
        fingerprint: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LegacyProjectSelectorKindV1 {
    Project,
    ProjectAndRelativePath,
    AbsolutePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerSnapshotRowValueV1 {
    LegacyProjectSelector {
        selector_kind: LegacyProjectSelectorKindV1,
        literal_selector: String,
    },
    InventoryTarget {
        project_id: String,
        target_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSnapshotRowV1 {
    pub stable_row_id: String,
    pub value: OwnerSnapshotRowValueV1,
}

impl OwnerSnapshotRowV1 {
    pub fn legacy_selector(
        stable_row_id: impl Into<String>,
        selector_kind: LegacyProjectSelectorKindV1,
        literal_selector: impl Into<String>,
    ) -> Self {
        Self {
            stable_row_id: stable_row_id.into(),
            value: OwnerSnapshotRowValueV1::LegacyProjectSelector {
                selector_kind,
                literal_selector: literal_selector.into(),
            },
        }
    }

    pub fn inventory_target(
        stable_row_id: impl Into<String>,
        project_id: impl Into<String>,
        target_sha256: impl Into<String>,
    ) -> Self {
        Self {
            stable_row_id: stable_row_id.into(),
            value: OwnerSnapshotRowValueV1::InventoryTarget {
                project_id: project_id.into(),
                target_sha256: target_sha256.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSubsourceSnapshotV1 {
    pub subsource_id: String,
    pub state: OwnerSnapshotStateV1,
    pub row_ids: BTreeSet<String>,
    pub row_count: u64,
    pub canonical_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSnapshotV1 {
    pub source_id: String,
    pub state: OwnerSnapshotStateV1,
    pub subsources: Vec<OwnerSubsourceSnapshotV1>,
    pub rows: Vec<OwnerSnapshotRowV1>,
    pub row_count: u64,
    pub canonical_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSnapshotError {
    pub code: &'static str,
}

impl std::fmt::Display for OwnerSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for OwnerSnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedOwnerBytesV1 {
    pub state: OwnerSnapshotStateV1,
    pub bytes: Option<Vec<u8>>,
}

pub fn capture_regular_file_nofollow(
    path: &Path,
    source_id: &str,
    subsource_id: &str,
    max_bytes: usize,
) -> CapturedOwnerBytesV1 {
    let missing = || OwnerSnapshotStateV1::Missing {
        fingerprint: state_fingerprint("missing", source_id, subsource_id),
    };
    let corrupt = |diagnostic_code: &str| OwnerSnapshotStateV1::Corrupt {
        diagnostic_code: diagnostic_code.to_string(),
        fingerprint: state_fingerprint(diagnostic_code, source_id, subsource_id),
    };
    let Some(parent) = path.parent() else {
        return CapturedOwnerBytesV1 {
            state: corrupt("owner_path_has_no_parent"),
            bytes: None,
        };
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return CapturedOwnerBytesV1 {
            state: corrupt("owner_filename_invalid"),
            bytes: None,
        };
    };
    let directory = match crate::json_store::NofollowDirectory::open_existing(parent) {
        Ok(Some(directory)) => directory,
        Ok(None) => {
            return CapturedOwnerBytesV1 {
                state: missing(),
                bytes: None,
            };
        }
        Err(_) => {
            return CapturedOwnerBytesV1 {
                state: corrupt("owner_parent_unsafe"),
                bytes: None,
            };
        }
    };
    match directory.read_regular(name, max_bytes, "owner source") {
        Ok(Some(bytes)) => CapturedOwnerBytesV1 {
            state: OwnerSnapshotStateV1::Present {
                content_sha256: sha256_hex(&bytes),
                byte_len: bytes.len() as u64,
            },
            bytes: Some(bytes),
        },
        Ok(None) => CapturedOwnerBytesV1 {
            state: missing(),
            bytes: None,
        },
        Err(_) => CapturedOwnerBytesV1 {
            state: corrupt("owner_source_unreadable"),
            bytes: None,
        },
    }
}

pub fn capture_json_owner(
    path: &Path,
    source_id: &str,
    subsource_id: &str,
    limits: OwnerSnapshotLimitsV1,
    decode: impl FnOnce(&[u8]) -> Result<Vec<OwnerSnapshotRowV1>, ()>,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    validate_limits(limits)?;
    let captured =
        capture_regular_file_nofollow(path, source_id, subsource_id, limits.max_source_bytes);
    let Some(bytes) = captured.bytes else {
        return build_owner_snapshot(
            source_id,
            vec![owner_subsource(subsource_id, captured.state, &[])],
            Vec::new(),
            limits,
        );
    };
    let rows = match decode(&bytes) {
        Ok(rows) => rows,
        Err(()) => {
            return corrupt_owner_snapshot(source_id, subsource_id, "owner_source_invalid", limits);
        }
    };
    finalize_owner_snapshot(
        source_id,
        subsource_id,
        vec![owner_subsource(subsource_id, captured.state, &rows)],
        rows,
        limits,
    )
}

// ---------------------------------------------------------------------------
// Row stamping: the write-back half of the owner snapshot contract
// ---------------------------------------------------------------------------
//
// Capture answers "which rows still carry a legacy path selector". Stamping is
// its inverse: write the stable `project_id` onto one such row. The two halves
// live side by side deliberately, because they must agree on the row identity
// (`stable_row_id`) that the backfill ledger keys on.

/// What stamping one durable-store row did.
///
/// There is no `Skipped` variant. A row that cannot be stamped is a typed
/// error, never a quiet success: absence-as-success is exactly how a partial
/// backfill would report itself complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerRowStampOutcomeV1 {
    /// The row carried no project id and now carries the requested one.
    Stamped,
    /// The row already carried EXACTLY the requested project id, so the write
    /// was elided. This is what makes re-applying a torn backfill safe: the
    /// already-completed prefix reports `AlreadyStamped` instead of erroring or
    /// double-writing.
    AlreadyStamped,
}

/// A typed refusal from the stamping path. `code` is a stable diagnostic token,
/// matching the owner snapshot error convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRowStampError {
    pub code: &'static str,
}

impl OwnerRowStampError {
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }
}

impl std::fmt::Display for OwnerRowStampError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for OwnerRowStampError {}

/// The requested row does not exist in the owner source.
pub const OWNER_ROW_ABSENT: &str = "owner_row_absent";
/// The row already carries a DIFFERENT stable project id. Never overwritten.
pub const OWNER_ROW_PROJECT_ID_CONFLICT: &str = "owner_row_project_id_conflict";
/// The owner source file does not exist.
pub const OWNER_SOURCE_MISSING: &str = "owner_source_missing";
/// The owner source exists but could not be decoded.
pub const OWNER_SOURCE_INVALID: &str = "owner_source_invalid";
/// The owner source could not be read (unsafe parent, symlink, oversize).
pub const OWNER_SOURCE_UNREADABLE: &str = "owner_source_unreadable";
/// The rewritten owner source could not be committed to disk.
pub const OWNER_SOURCE_UNWRITABLE: &str = "owner_source_unwritable";
/// The owner source CHANGED between the stamper's read and its atomic
/// replacement, so the write was abandoned rather than clobbering it.
///
/// Deliberately not [`OWNER_SOURCE_UNWRITABLE`]: "unwritable" implies a
/// permissions or disk problem, while this states the fact - the source moved.
/// It is current-state divergence, so the backfill maps it onto the STALENESS
/// family rather than artifact invalidity (adjudication Q-E4's principle,
/// applied to a third diagnostic beyond the two it originally enumerated).
pub const OWNER_SOURCE_MOVED: &str = "owner_source_moved";
/// The caller supplied an empty or whitespace-only project id.
pub const OWNER_PROJECT_ID_INVALID: &str = "owner_project_id_invalid";

/// Whether a row needs a write, decided from its CURRENT project id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStampDecisionV1 {
    /// Row is unstamped: write the project id.
    Write,
    /// Row already carries the requested project id: elide the write.
    AlreadyStamped,
}

/// The single implementation of stamp idempotency, shared by every owner.
///
/// Every owner crate routes its row decision through this function rather than
/// re-deriving the three-way rule, so "an already-stamped row is a no-op, a
/// differently-stamped row is a refusal, and nothing is ever silently
/// overwritten" has exactly one definition to audit and to change.
pub fn decide_row_stamp(
    existing_project_id: Option<&str>,
    project_id: &str,
) -> Result<RowStampDecisionV1, OwnerRowStampError> {
    if project_id.trim().is_empty() {
        return Err(OwnerRowStampError::new(OWNER_PROJECT_ID_INVALID));
    }
    match existing_project_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        None => Ok(RowStampDecisionV1::Write),
        Some(existing) if existing == project_id => Ok(RowStampDecisionV1::AlreadyStamped),
        // A row bound to another project is a real inconsistency between the
        // resolution artifact and durable state. Overwriting it would silently
        // move content between projects, so the backfill refuses and surfaces
        // it instead.
        Some(_) => Err(OwnerRowStampError::new(OWNER_ROW_PROJECT_ID_CONFLICT)),
    }
}

/// What an owner's decode-and-stamp closure decided to do with the source.
pub enum OwnerSourceEditV1 {
    /// The target row was already stamped; leave the bytes untouched.
    AlreadyStamped,
    /// Commit these bytes as the new owner source.
    Rewrite(Vec<u8>),
}

/// Locked read-modify-write plumbing for stamping one row of a JSON owner
/// source, mirroring [`capture_json_owner`] on the read side.
///
/// The owner crate keeps ownership of its own schema: `edit` receives the raw
/// source bytes and returns either the rewritten bytes or `AlreadyStamped`.
/// This function owns only what every owner must share: the exclusive store
/// lock, the nofollow read, the missing/unreadable refusals, and the atomic
/// fsynced replace. Holding the lock across BOTH the read and the write is
/// load-bearing: a stamp is a read-modify-write, so a concurrent writer between
/// the two would be lost.
pub fn stamp_json_owner_row(
    store_path: &Path,
    source_id: &str,
    subsource_id: &str,
    limits: OwnerSnapshotLimitsV1,
    edit: impl FnOnce(&[u8]) -> Result<OwnerSourceEditV1, OwnerRowStampError>,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    validate_limits(limits).map_err(|error| OwnerRowStampError::new(error.code))?;
    crate::json_store::with_store_lock(store_path, || {
        let captured = capture_regular_file_nofollow(
            store_path,
            source_id,
            subsource_id,
            limits.max_source_bytes,
        );
        let Some(bytes) = captured.bytes else {
            // An absent owner source is a refusal, not an empty success: the
            // resolution named a row that this store cannot produce.
            let code = match captured.state {
                OwnerSnapshotStateV1::Missing { .. } => OWNER_SOURCE_MISSING,
                _ => OWNER_SOURCE_UNREADABLE,
            };
            return Ok(Err(OwnerRowStampError::new(code)));
        };
        let edited = match edit(&bytes) {
            Ok(edited) => edited,
            Err(error) => return Ok(Err(error)),
        };
        let rewritten = match edited {
            OwnerSourceEditV1::AlreadyStamped => {
                return Ok(Ok(OwnerRowStampOutcomeV1::AlreadyStamped));
            }
            OwnerSourceEditV1::Rewrite(rewritten) => rewritten,
        };
        match crate::json_store::atomic_write_bytes_locked(store_path, &rewritten) {
            Ok(()) => Ok(Ok(OwnerRowStampOutcomeV1::Stamped)),
            Err(_) => Ok(Err(OwnerRowStampError::new(OWNER_SOURCE_UNWRITABLE))),
        }
    })
    // A lock failure is indistinguishable from an unwritable source from the
    // caller's perspective: neither committed anything.
    .unwrap_or_else(|_| Err(OwnerRowStampError::new(OWNER_SOURCE_UNWRITABLE)))
}

/// Decode helper for owners whose source is a single JSON document: maps a
/// serde failure onto the shared `owner_source_invalid` refusal.
pub fn decode_owner_source<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, OwnerRowStampError> {
    serde_json::from_slice(bytes).map_err(|_| OwnerRowStampError::new(OWNER_SOURCE_INVALID))
}

/// Re-encode helper preserving the repo's pretty-with-trailing-newline JSON
/// convention, so a stamp does not reformat an otherwise untouched store.
pub fn encode_owner_source<T: serde::Serialize>(
    value: &T,
) -> Result<OwnerSourceEditV1, OwnerRowStampError> {
    crate::json_store::to_vec_pretty_newline(value)
        .map(OwnerSourceEditV1::Rewrite)
        .map_err(|_| OwnerRowStampError::new(OWNER_SOURCE_UNWRITABLE))
}

/// The name of the durable field every owner stamps.
pub const OWNER_ROW_PROJECT_ID_FIELD: &str = "project_id";

/// Apply the stamp decision to one row object IN PLACE.
///
/// Owners stamp through `serde_json::Value` rather than round-tripping their
/// typed schema on purpose. A typed round-trip silently drops any field the
/// compiled struct does not know about, so stamping a store written by a newer
/// binary would delete data as a side effect of adding one field. Editing the
/// value tree touches the target field and leaves every other byte of meaning
/// intact.
pub fn stamp_row_object(
    row: &mut serde_json::Value,
    project_id: &str,
) -> Result<RowStampDecisionV1, OwnerRowStampError> {
    let object = row
        .as_object_mut()
        .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_INVALID))?;
    let existing = match object.get(OWNER_ROW_PROJECT_ID_FIELD) {
        None => None,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(existing)) => Some(existing.as_str()),
        // A present-but-not-a-string project id is a corrupt row, not an
        // unstamped one. Treating it as absent would overwrite it.
        Some(_) => return Err(OwnerRowStampError::new(OWNER_SOURCE_INVALID)),
    };
    let decision = decide_row_stamp(existing, project_id)?;
    if decision == RowStampDecisionV1::Write {
        object.insert(
            OWNER_ROW_PROJECT_ID_FIELD.to_string(),
            serde_json::Value::String(project_id.to_string()),
        );
    }
    Ok(decision)
}

/// Locate one row inside a top-level array field, matched on its id field.
pub fn find_json_array_row_mut<'a>(
    document: &'a mut serde_json::Value,
    array_field: &str,
    id_field: &str,
    source_row_id: &str,
) -> Option<&'a mut serde_json::Value> {
    document
        .get_mut(array_field)?
        .as_array_mut()?
        .iter_mut()
        .find(|row| row.get(id_field).and_then(serde_json::Value::as_str) == Some(source_row_id))
}

/// Stamp one row of an owner whose source is a top-level object holding an
/// array of row objects keyed by an id field. This covers most central JSON
/// stores (knowledge entries, gaps, threads, notes, pins, roadmap items).
pub fn stamp_json_array_row(
    bytes: &[u8],
    array_field: &str,
    id_field: &str,
    source_row_id: &str,
    project_id: &str,
) -> Result<OwnerSourceEditV1, OwnerRowStampError> {
    let mut document: serde_json::Value = decode_owner_source(bytes)?;
    let row = find_json_array_row_mut(&mut document, array_field, id_field, source_row_id)
        .ok_or_else(|| OwnerRowStampError::new(OWNER_ROW_ABSENT))?;
    match stamp_row_object(row, project_id)? {
        RowStampDecisionV1::AlreadyStamped => Ok(OwnerSourceEditV1::AlreadyStamped),
        RowStampDecisionV1::Write => encode_owner_source(&document),
    }
}

/// Stamp one row of an owner whose source is a top-level object holding a MAP
/// of row objects, where the backfill's stable row id is derived from the row's
/// own fields rather than the map key (the Slack binding shape).
pub fn stamp_json_map_row(
    bytes: &[u8],
    map_field: &str,
    source_row_id: &str,
    project_id: &str,
    row_id_of: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<OwnerSourceEditV1, OwnerRowStampError> {
    let mut document: serde_json::Value = decode_owner_source(bytes)?;
    let rows = document
        .get_mut(map_field)
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| OwnerRowStampError::new(OWNER_ROW_ABSENT))?;
    let row = rows
        .values_mut()
        .find(|row| row_id_of(row).as_deref() == Some(source_row_id))
        .ok_or_else(|| OwnerRowStampError::new(OWNER_ROW_ABSENT))?;
    match stamp_row_object(row, project_id)? {
        RowStampDecisionV1::AlreadyStamped => Ok(OwnerSourceEditV1::AlreadyStamped),
        RowStampDecisionV1::Write => encode_owner_source(&document),
    }
}

/// Stamp an owner whose source file IS the row (one JSON document per record:
/// the packet, whiteboard, and artifact-metadata shape). `source_row_id` is
/// verified against the document's own id field so a resolution cannot stamp a
/// record it did not name.
pub fn stamp_json_document_row(
    bytes: &[u8],
    id_field: Option<(&str, &str)>,
    project_id: &str,
) -> Result<OwnerSourceEditV1, OwnerRowStampError> {
    let mut document: serde_json::Value = decode_owner_source(bytes)?;
    if let Some((id_field, source_row_id)) = id_field
        && document.get(id_field).and_then(serde_json::Value::as_str) != Some(source_row_id)
    {
        return Err(OwnerRowStampError::new(OWNER_ROW_ABSENT));
    }
    match stamp_row_object(&mut document, project_id)? {
        RowStampDecisionV1::AlreadyStamped => Ok(OwnerSourceEditV1::AlreadyStamped),
        RowStampDecisionV1::Write => encode_owner_source(&document),
    }
}

/// Stamp one row of an owner whose source is a TREE of JSON documents, one
/// record per file (the packet, whiteboard, artifact-metadata, and proposal
/// shape). Mirrors [`capture_stable_regular_tree_nofollow`] on the read side.
///
/// `row_id_of` reconstructs the backfill's stable row id from a document and
/// its subsource id, exactly as the owner's capture does. That id is
/// deliberately recomputed a SECOND time under the file lock: the walk that
/// locates the file is unlocked, so re-verifying before writing is what stops a
/// concurrent edit from redirecting the stamp onto a different record.
pub fn stamp_json_tree_row(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool + Copy,
    row_id_of: impl Fn(&str, &serde_json::Value) -> Option<String>,
    source_row_id: &str,
    project_id: &str,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    let captures = capture_stable_regular_tree_nofollow(root, source_id, limits, include)
        .map_err(|error| OwnerRowStampError::new(error.code))?;
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id(source_id, &relative);
        let Some(bytes) = captured.bytes else {
            continue;
        };
        let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if row_id_of(&subsource_id, &document).as_deref() != Some(source_row_id) {
            continue;
        }
        return stamp_json_owner_row(
            &root.join(&relative),
            source_id,
            &subsource_id,
            limits,
            |bytes| {
                let document: serde_json::Value = decode_owner_source(bytes)?;
                if row_id_of(&subsource_id, &document).as_deref() != Some(source_row_id) {
                    return Err(OwnerRowStampError::new(OWNER_ROW_ABSENT));
                }
                stamp_json_document_row(bytes, None, project_id)
            },
        );
    }
    Err(OwnerRowStampError::new(OWNER_ROW_ABSENT))
}

/// Minimal dependency-safe projection of the root daemon's persisted
/// `tasks.json` schema. The orchestration cwd is the only field that is a
/// legacy path selector; all other task payload remains owned by the root
/// runtime.
pub fn capture_legacy_task_owner_snapshot(
    tasks_path: &Path,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    #[derive(Deserialize)]
    struct PersistedTaskSelector {
        id: String,
        #[serde(default)]
        cwd: Option<String>,
    }

    capture_json_owner(tasks_path, "task", "task:central-json", limits, |bytes| {
        let tasks: Vec<PersistedTaskSelector> = serde_json::from_slice(bytes).map_err(|_| ())?;
        Ok(tasks
            .into_iter()
            .filter_map(|task| {
                let selector = task.cwd?.trim().to_string();
                (!selector.is_empty()).then(|| {
                    OwnerSnapshotRowV1::legacy_selector(
                        task.id,
                        LegacyProjectSelectorKindV1::AbsolutePath,
                        selector,
                    )
                })
            })
            .collect())
    })
}

/// Stamp one task row, the write half of
/// [`capture_legacy_task_owner_snapshot`].
///
/// `tasks.json` is a TOP-LEVEL ARRAY rather than an object holding a named
/// array, so it cannot use [`stamp_json_array_row`]; the row lookup differs and
/// nothing else does. Everything that matters is still shared: the exclusive
/// store lock, the nofollow read, the atomic fsynced replace, and the one
/// three-way stamp decision in [`stamp_row_object`].
///
/// Stamping through `serde_json::Value` is what preserves a field this binary's
/// `PersistedTask` does not know about. A typed round-trip would silently drop
/// it, so a newer daemon's task record would lose data as a side effect of the
/// backfill adding one field.
pub fn stamp_legacy_task_owner_row(
    tasks_path: &Path,
    source_row_id: &str,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    stamp_json_owner_row(tasks_path, "task", "task:central-json", limits, |bytes| {
        let mut document: serde_json::Value = decode_owner_source(bytes)?;
        let row = document
            .as_array_mut()
            .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_INVALID))?
            .iter_mut()
            .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(source_row_id))
            .ok_or_else(|| OwnerRowStampError::new(OWNER_ROW_ABSENT))?;
        match stamp_row_object(row, project_id)? {
            RowStampDecisionV1::AlreadyStamped => Ok(OwnerSourceEditV1::AlreadyStamped),
            RowStampDecisionV1::Write => encode_owner_source(&document),
        }
    })
}

/// Minimal dependency-safe projection of the root consultant proposal store.
/// Current proposal records carry stable project ids through their owning
/// consultant instance, not literal paths. Optional legacy path fields are
/// nevertheless captured if an older record contains them.
pub fn capture_legacy_proposal_owner_snapshot(
    proposals_root: &Path,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    #[derive(Deserialize)]
    struct PersistedProposalSelector {
        id: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        project_dir: Option<String>,
    }

    match std::fs::symlink_metadata(proposals_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("proposal", "proposal:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "proposal",
                "proposal:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures =
        match capture_stable_regular_tree_nofollow(proposals_root, "proposal", limits, |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        }) {
            Ok(captures) => captures,
            Err(error) => {
                return corrupt_owner_snapshot("proposal", "proposal:root", error.code, limits);
            }
        };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "proposal",
            vec![owner_subsource("proposal:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("proposal", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "proposal",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let proposal: PersistedProposalSelector = match serde_json::from_slice(&bytes) {
            Ok(proposal) => proposal,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "proposal",
                    &subsource_id,
                    "owner_source_invalid",
                    limits,
                );
            }
        };
        let selector = proposal
            .project_dir
            .or(proposal.project)
            .map(|selector| selector.trim().to_string())
            .filter(|selector| !selector.is_empty());
        let subsource_rows = selector
            .map(|selector| {
                vec![OwnerSnapshotRowV1::legacy_selector(
                    format!("{subsource_id}:{}", proposal.id),
                    LegacyProjectSelectorKindV1::Project,
                    selector,
                )]
            })
            .unwrap_or_default();
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot("proposal", "proposal:root", subsources, rows, limits)
}

/// Stamp one legacy proposal record with its stable project id, the write-back
/// inverse of [`capture_legacy_proposal_owner_snapshot`].
pub fn stamp_legacy_proposal_owner_row(
    proposals_root: &Path,
    source_row_id: &str,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    stamp_json_tree_row(
        proposals_root,
        "proposal",
        limits,
        |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        },
        |subsource_id, document| {
            document
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(|id| format!("{subsource_id}:{id}"))
        },
        source_row_id,
        project_id,
    )
}

pub fn capture_stable_regular_tree_nofollow(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool + Copy,
) -> Result<Vec<(PathBuf, CapturedOwnerBytesV1)>, OwnerSnapshotError> {
    let authority = crate::json_store::NofollowDirectory::open_existing(root)
        .map_err(|_| OwnerSnapshotError {
            code: "owner_tree_unsafe",
        })?
        .ok_or(OwnerSnapshotError {
            code: "owner_tree_changed_during_capture",
        })?;
    let mut prior = capture_regular_tree_nofollow(root, source_id, limits, include)?;
    for _ in 0..3 {
        let current = capture_regular_tree_nofollow(root, source_id, limits, include)?;
        if current == prior {
            authority
                .ensure_still_current()
                .map_err(|_| OwnerSnapshotError {
                    code: "owner_tree_changed_during_capture",
                })?;
            return Ok(current);
        }
        prior = current;
    }
    Err(OwnerSnapshotError {
        code: "owner_tree_changed_during_capture",
    })
}

pub fn capture_regular_tree_nofollow(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool,
) -> Result<Vec<(PathBuf, CapturedOwnerBytesV1)>, OwnerSnapshotError> {
    validate_limits(limits)?;
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(OwnerSnapshotError {
                code: "owner_tree_unreadable",
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(OwnerSnapshotError {
            code: "owner_tree_unsafe",
        });
    }

    let mut pending = vec![PathBuf::new()];
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    while let Some(relative_dir) = pending.pop() {
        let absolute_dir = root.join(&relative_dir);
        let mut entries = std::fs::read_dir(&absolute_dir)
            .map_err(|_| OwnerSnapshotError {
                code: "owner_tree_unreadable",
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| OwnerSnapshotError {
                code: "owner_tree_unreadable",
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let relative = relative_dir.join(&name);
            if relative
                .components()
                .any(|component| {
                    !matches!(component, Component::Normal(name) if name.to_str().is_some())
                })
            {
                return Err(OwnerSnapshotError {
                    code: "owner_tree_entry_invalid",
                });
            }
            let file_type = entry.file_type().map_err(|_| OwnerSnapshotError {
                code: "owner_tree_unreadable",
            })?;
            if file_type.is_symlink() {
                return Err(OwnerSnapshotError {
                    code: "owner_tree_symlink",
                });
            }
            if file_type.is_dir() {
                pending.push(relative);
                continue;
            }
            if !file_type.is_file() || !include(&relative) {
                continue;
            }
            if files.len() >= limits.max_subsources {
                return Err(OwnerSnapshotError {
                    code: "owner_subsource_limit",
                });
            }
            let subsource_id = stable_subsource_id(source_id, &relative);
            let captured = capture_regular_file_nofollow(
                &entry.path(),
                source_id,
                &subsource_id,
                limits.max_source_bytes.saturating_sub(total_bytes),
            );
            if let OwnerSnapshotStateV1::Present { byte_len, .. } = &captured.state {
                total_bytes =
                    total_bytes
                        .checked_add(*byte_len as usize)
                        .ok_or(OwnerSnapshotError {
                            code: "owner_source_byte_limit",
                        })?;
                if total_bytes > limits.max_source_bytes {
                    return Err(OwnerSnapshotError {
                        code: "owner_source_byte_limit",
                    });
                }
            }
            files.push((relative, captured));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

pub fn owner_subsource(
    subsource_id: impl Into<String>,
    state: OwnerSnapshotStateV1,
    rows: &[OwnerSnapshotRowV1],
) -> OwnerSubsourceSnapshotV1 {
    let subsource_id = subsource_id.into();
    let row_ids = rows
        .iter()
        .map(|row| row.stable_row_id.clone())
        .collect::<BTreeSet<_>>();
    let canonical_sha256 = rows_commitment(&subsource_id, rows);
    OwnerSubsourceSnapshotV1 {
        subsource_id,
        state,
        row_count: row_ids.len() as u64,
        row_ids,
        canonical_sha256,
    }
}

pub fn build_owner_snapshot(
    source_id: impl Into<String>,
    mut subsources: Vec<OwnerSubsourceSnapshotV1>,
    mut rows: Vec<OwnerSnapshotRowV1>,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    validate_limits(limits)?;
    let source_id = source_id.into();
    if source_id.is_empty() || source_id.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(OwnerSnapshotError {
            code: "owner_source_id_invalid",
        });
    }
    if subsources.len() > limits.max_subsources {
        return Err(OwnerSnapshotError {
            code: "owner_subsource_limit",
        });
    }
    if rows.len() > limits.max_rows {
        return Err(OwnerSnapshotError {
            code: "owner_row_limit",
        });
    }
    subsources.sort_by(|left, right| left.subsource_id.cmp(&right.subsource_id));
    if subsources
        .windows(2)
        .any(|pair| pair[0].subsource_id == pair[1].subsource_id)
    {
        return Err(OwnerSnapshotError {
            code: "owner_subsource_duplicate",
        });
    }
    for subsource in &subsources {
        if subsource.subsource_id.is_empty()
            || subsource
                .subsource_id
                .bytes()
                .any(|byte| byte.is_ascii_control())
            || subsource.row_count != subsource.row_ids.len() as u64
            || !valid_sha256(&subsource.canonical_sha256)
            || !valid_state(&subsource.state)
        {
            return Err(OwnerSnapshotError {
                code: "owner_subsource_invalid",
            });
        }
    }
    rows.sort_by(|left, right| left.stable_row_id.cmp(&right.stable_row_id));
    if rows
        .windows(2)
        .any(|pair| pair[0].stable_row_id == pair[1].stable_row_id)
    {
        return Err(OwnerSnapshotError {
            code: "owner_row_duplicate",
        });
    }
    for row in &rows {
        if row.stable_row_id.is_empty()
            || row
                .stable_row_id
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            return Err(OwnerSnapshotError {
                code: "owner_row_id_invalid",
            });
        }
        if let OwnerSnapshotRowValueV1::LegacyProjectSelector {
            literal_selector, ..
        } = &row.value
            && (literal_selector.is_empty()
                || literal_selector.len() > limits.max_selector_bytes
                || literal_selector
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control()))
        {
            return Err(OwnerSnapshotError {
                code: "owner_selector_invalid",
            });
        }
        if let OwnerSnapshotRowValueV1::InventoryTarget {
            project_id,
            target_sha256,
        } = &row.value
            && (project_id.is_empty()
                || project_id.bytes().any(|byte| byte.is_ascii_control())
                || !valid_sha256(target_sha256))
        {
            return Err(OwnerSnapshotError {
                code: "owner_inventory_target_invalid",
            });
        }
    }
    let row_ids = rows
        .iter()
        .map(|row| row.stable_row_id.as_str())
        .collect::<BTreeSet<_>>();
    let subsource_row_ids = subsources
        .iter()
        .flat_map(|subsource| subsource.row_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    if row_ids != subsource_row_ids {
        return Err(OwnerSnapshotError {
            code: "owner_subsource_rows_mismatch",
        });
    }
    let state = aggregate_state(&source_id, &subsources);
    let canonical_sha256 = snapshot_commitment(&source_id, &subsources, &rows);
    Ok(OwnerSnapshotV1 {
        source_id,
        state,
        row_count: rows.len() as u64,
        subsources,
        rows,
        canonical_sha256,
    })
}

pub fn finalize_owner_snapshot(
    source_id: &str,
    corrupt_subsource_id: &str,
    subsources: Vec<OwnerSubsourceSnapshotV1>,
    rows: Vec<OwnerSnapshotRowV1>,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    match build_owner_snapshot(source_id, subsources, rows, limits) {
        Ok(snapshot) => Ok(snapshot),
        Err(error) if error.code != "owner_snapshot_limits_invalid" => {
            corrupt_owner_snapshot(source_id, corrupt_subsource_id, error.code, limits)
        }
        Err(error) => Err(error),
    }
}

pub fn missing_owner_snapshot(
    source_id: &str,
    subsource_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    let state = OwnerSnapshotStateV1::Missing {
        fingerprint: state_fingerprint("missing", source_id, subsource_id),
    };
    build_owner_snapshot(
        source_id,
        vec![owner_subsource(subsource_id, state, &[])],
        Vec::new(),
        limits,
    )
}

pub fn corrupt_owner_snapshot(
    source_id: &str,
    subsource_id: &str,
    diagnostic_code: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    let state = OwnerSnapshotStateV1::Corrupt {
        diagnostic_code: diagnostic_code.to_string(),
        fingerprint: state_fingerprint(diagnostic_code, source_id, subsource_id),
    };
    build_owner_snapshot(
        source_id,
        vec![owner_subsource(subsource_id, state, &[])],
        Vec::new(),
        limits,
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn stable_subsource_id(source_id: &str, relative_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, source_id.as_bytes());
    for component in relative_path.components() {
        if let Component::Normal(component) = component {
            hash_field(&mut hasher, component.to_string_lossy().as_bytes());
        }
    }
    format!("{source_id}:{}", hex::encode(hasher.finalize()))
}

fn validate_limits(limits: OwnerSnapshotLimitsV1) -> Result<(), OwnerSnapshotError> {
    if limits.max_source_bytes == 0
        || limits.max_subsources == 0
        || limits.max_rows == 0
        || limits.max_selector_bytes == 0
    {
        return Err(OwnerSnapshotError {
            code: "owner_snapshot_limits_invalid",
        });
    }
    Ok(())
}

fn valid_state(state: &OwnerSnapshotStateV1) -> bool {
    match state {
        OwnerSnapshotStateV1::Present { content_sha256, .. } => valid_sha256(content_sha256),
        OwnerSnapshotStateV1::Missing { fingerprint } => valid_sha256(fingerprint),
        OwnerSnapshotStateV1::Corrupt {
            diagnostic_code,
            fingerprint,
        } => {
            !diagnostic_code.is_empty()
                && diagnostic_code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                && valid_sha256(fingerprint)
        }
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn aggregate_state(
    source_id: &str,
    subsources: &[OwnerSubsourceSnapshotV1],
) -> OwnerSnapshotStateV1 {
    if let Some(OwnerSubsourceSnapshotV1 {
        state: OwnerSnapshotStateV1::Corrupt {
            diagnostic_code, ..
        },
        ..
    }) = subsources
        .iter()
        .find(|subsource| matches!(&subsource.state, OwnerSnapshotStateV1::Corrupt { .. }))
    {
        return OwnerSnapshotStateV1::Corrupt {
            diagnostic_code: diagnostic_code.clone(),
            fingerprint: state_fingerprint(diagnostic_code, source_id, "aggregate"),
        };
    }
    if subsources
        .iter()
        .any(|subsource| matches!(&subsource.state, OwnerSnapshotStateV1::Missing { .. }))
    {
        return OwnerSnapshotStateV1::Missing {
            fingerprint: state_fingerprint("missing", source_id, "aggregate"),
        };
    }
    let mut hasher = Sha256::new();
    let mut byte_len = 0u64;
    for subsource in subsources {
        hash_field(&mut hasher, subsource.subsource_id.as_bytes());
        hash_field(&mut hasher, subsource.canonical_sha256.as_bytes());
        if let OwnerSnapshotStateV1::Present {
            content_sha256,
            byte_len: subsource_len,
        } = &subsource.state
        {
            hash_field(&mut hasher, content_sha256.as_bytes());
            byte_len = byte_len.saturating_add(*subsource_len);
        }
    }
    OwnerSnapshotStateV1::Present {
        content_sha256: hex::encode(hasher.finalize()),
        byte_len,
    }
}

fn state_fingerprint(kind: &str, source_id: &str, subsource_id: &str) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, kind.as_bytes());
    hash_field(&mut hasher, source_id.as_bytes());
    hash_field(&mut hasher, subsource_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn rows_commitment(source_id: &str, rows: &[OwnerSnapshotRowV1]) -> String {
    let mut ordered = rows.to_vec();
    ordered.sort_by(|left, right| left.stable_row_id.cmp(&right.stable_row_id));
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, source_id.as_bytes());
    for row in &ordered {
        hash_row(&mut hasher, row);
    }
    hex::encode(hasher.finalize())
}

fn snapshot_commitment(
    source_id: &str,
    subsources: &[OwnerSubsourceSnapshotV1],
    rows: &[OwnerSnapshotRowV1],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, source_id.as_bytes());
    for subsource in subsources {
        hash_field(&mut hasher, subsource.subsource_id.as_bytes());
        hash_state(&mut hasher, &subsource.state);
        hash_field(&mut hasher, subsource.canonical_sha256.as_bytes());
    }
    for row in rows {
        hash_row(&mut hasher, row);
    }
    hex::encode(hasher.finalize())
}

fn hash_state(hasher: &mut Sha256, state: &OwnerSnapshotStateV1) {
    match state {
        OwnerSnapshotStateV1::Present {
            content_sha256,
            byte_len,
        } => {
            hash_field(hasher, b"present");
            hash_field(hasher, content_sha256.as_bytes());
            hash_field(hasher, &byte_len.to_be_bytes());
        }
        OwnerSnapshotStateV1::Missing { fingerprint } => {
            hash_field(hasher, b"missing");
            hash_field(hasher, fingerprint.as_bytes());
        }
        OwnerSnapshotStateV1::Corrupt {
            diagnostic_code,
            fingerprint,
        } => {
            hash_field(hasher, b"corrupt");
            hash_field(hasher, diagnostic_code.as_bytes());
            hash_field(hasher, fingerprint.as_bytes());
        }
    }
}

fn hash_row(hasher: &mut Sha256, row: &OwnerSnapshotRowV1) {
    hash_field(hasher, row.stable_row_id.as_bytes());
    match &row.value {
        OwnerSnapshotRowValueV1::LegacyProjectSelector {
            selector_kind,
            literal_selector,
        } => {
            hash_field(hasher, b"legacy_selector");
            hash_field(
                hasher,
                match selector_kind {
                    LegacyProjectSelectorKindV1::Project => b"project",
                    LegacyProjectSelectorKindV1::ProjectAndRelativePath => {
                        b"project_and_relative_path"
                    }
                    LegacyProjectSelectorKindV1::AbsolutePath => b"absolute_path",
                },
            );
            hash_field(hasher, literal_selector.as_bytes());
        }
        OwnerSnapshotRowValueV1::InventoryTarget {
            project_id,
            target_sha256,
        } => {
            hash_field(hasher, b"inventory_target");
            hash_field(hasher, project_id.as_bytes());
            hash_field(hasher, target_sha256.as_bytes());
        }
    }
}

fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_commitment_is_canonical_and_carries_literal_selector() {
        let limits = OwnerSnapshotLimitsV1::default();
        let a = OwnerSnapshotRowV1::legacy_selector(
            "a",
            LegacyProjectSelectorKindV1::Project,
            "/repo/a",
        );
        let b = OwnerSnapshotRowV1::legacy_selector(
            "b",
            LegacyProjectSelectorKindV1::AbsolutePath,
            "/repo/b/file",
        );
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b"source"),
            byte_len: 6,
        };
        let first = build_owner_snapshot(
            "owner",
            vec![owner_subsource(
                "owner:file",
                state.clone(),
                &[a.clone(), b.clone()],
            )],
            vec![b.clone(), a.clone()],
            limits,
        )
        .unwrap();
        let second = build_owner_snapshot(
            "owner",
            vec![owner_subsource("owner:file", state, &[b, a])],
            first.rows.clone(),
            limits,
        )
        .unwrap();
        assert_eq!(first.canonical_sha256, second.canonical_sha256);
        let changed = OwnerSnapshotRowV1::legacy_selector(
            "a",
            LegacyProjectSelectorKindV1::Project,
            "/repo/changed",
        );
        let changed_snapshot = build_owner_snapshot(
            "owner",
            vec![owner_subsource(
                "owner:file",
                OwnerSnapshotStateV1::Present {
                    content_sha256: sha256_hex(b"source"),
                    byte_len: 6,
                },
                &[changed.clone(), first.rows[1].clone()],
            )],
            vec![changed, first.rows[1].clone()],
            limits,
        )
        .unwrap();
        assert_ne!(first.canonical_sha256, changed_snapshot.canonical_sha256);
    }

    #[test]
    fn nofollow_capture_distinguishes_missing_present_and_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("owner.json");
        let missing = capture_regular_file_nofollow(&path, "owner", "owner:file", 32);
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        std::fs::write(&path, b"{}").unwrap();
        let present = capture_regular_file_nofollow(&path, "owner", "owner:file", 32);
        assert_eq!(present.bytes.as_deref(), Some(b"{}".as_slice()));
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(root.join("target"), &path).unwrap();
            let unsafe_source = capture_regular_file_nofollow(&path, "owner", "owner:file", 32);
            assert!(matches!(
                unsafe_source.state,
                OwnerSnapshotStateV1::Corrupt { .. }
            ));
        }
    }

    #[test]
    fn task_projection_is_read_only_bounded_and_exact() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("tasks.json");
        let missing =
            capture_legacy_task_owner_snapshot(&path, OwnerSnapshotLimitsV1::default()).unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!path.exists());

        std::fs::write(
            &path,
            br#"[
              {"id":"task-b","provider":"glm","cwd":null},
              {"id":"task-a","provider":"claude","cwd":"/repo/a"}
            ]"#,
        )
        .unwrap();
        let present =
            capture_legacy_task_owner_snapshot(&path, OwnerSnapshotLimitsV1::default()).unwrap();
        assert!(matches!(
            present.state,
            OwnerSnapshotStateV1::Present { .. }
        ));
        assert_eq!(present.row_count, 1);
        assert_eq!(present.rows[0].stable_row_id, "task-a");
        assert!(matches!(
            &present.rows[0].value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector {
                selector_kind: LegacyProjectSelectorKindV1::AbsolutePath,
                literal_selector,
            } if literal_selector == "/repo/a"
        ));

        let too_small = OwnerSnapshotLimitsV1 {
            max_source_bytes: 4,
            ..OwnerSnapshotLimitsV1::default()
        };
        let bounded = capture_legacy_task_owner_snapshot(&path, too_small).unwrap();
        assert!(matches!(
            bounded.state,
            OwnerSnapshotStateV1::Corrupt { .. }
        ));
    }

    #[test]
    fn proposal_projection_accepts_current_rows_and_captures_legacy_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let instance = root.join("bg-00000000-00000000");
        std::fs::create_dir(&instance).unwrap();
        std::fs::write(
            instance.join("P-1.json"),
            br#"{"id":"P-1","instance_id":"bg-00000000-00000000","kind":"packet","state":"pending","draft":{},"created_at":"now","updated_at":"now"}"#,
        )
        .unwrap();
        std::fs::write(
            instance.join("P-2.json"),
            br#"{"id":"P-2","project_dir":"/repo/legacy"}"#,
        )
        .unwrap();

        let snapshot =
            capture_legacy_proposal_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.subsources.len(), 2);
        assert_eq!(snapshot.row_count, 1);
        assert!(matches!(
            &snapshot.rows[0].value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector,
                ..
            } if literal_selector == "/repo/legacy"
        ));
    }
}

// ── Project-catalog row stamping (P6-B) ─────────────────────────────────────

#[cfg(test)]
mod proposal_owner_row_stamping {
    use super::*;

    struct Fixture {
        root: PathBuf,
        row_a: String,
        row_b: String,
        path_a: PathBuf,
        path_b: PathBuf,
    }

    fn document(id: &str, selector: &str, extra: bool) -> Vec<u8> {
        let future = if extra {
            r#", "future_field": {"kept": true}"#
        } else {
            ""
        };
        format!(
            r#"{{"id": "{id}", "project_dir": "{selector}"{future}}}
"#
        )
        .into_bytes()
    }

    /// The proposal row id combines the record's path-derived subsource id with
    /// its own id, so the test reconstructs it exactly as capture does.
    fn row_id(relative: &str, id: &str) -> String {
        format!(
            "{}:{id}",
            stable_subsource_id("proposal", Path::new(relative))
        )
    }

    fn write_fixture(dir: &tempfile::TempDir) -> Fixture {
        let root = dir.path().canonicalize().unwrap().join("proposals");
        std::fs::create_dir_all(&root).unwrap();
        let path_a = root.join("one.json");
        let path_b = root.join("two.json");
        std::fs::write(&path_a, document("pr1", "/legacy/path/one", true)).unwrap();
        std::fs::write(&path_b, document("pr2", "/legacy/path/two", false)).unwrap();
        Fixture {
            root,
            row_a: row_id("one.json", "pr1"),
            row_b: row_id("two.json", "pr2"),
            path_a,
            path_b,
        }
    }

    fn read_bytes(fixture: &Fixture, row: &str) -> Vec<u8> {
        let path = if row == fixture.row_a {
            &fixture.path_a
        } else {
            &fixture.path_b
        };
        std::fs::read(path).unwrap()
    }

    fn read_row(fixture: &Fixture, row: &str) -> serde_json::Value {
        serde_json::from_slice(&read_bytes(fixture, row)).unwrap()
    }

    fn stamp(
        fixture: &Fixture,
        row: &str,
        project_id: &str,
    ) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
        stamp_legacy_proposal_owner_row(
            &fixture.root,
            row,
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
        // The legacy selector is RETAINED for dual-read.
        assert_eq!(row["project_dir"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one record must not touch its neighbours.
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
        assert_eq!(read_bytes(&fixture, &fixture.row_a), after_first);
    }

    /// Never a silent overwrite.
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

    /// Absence is a refusal, never a success.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        let error = stamp(&fixture, "proposal:deadbeef:pr9", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create it.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("proposals");
        let fixture = Fixture {
            row_a: row_id("one.json", "pr1"),
            row_b: row_id("two.json", "pr2"),
            path_a: root.join("one.json"),
            path_b: root.join("two.json"),
            root,
        };

        assert!(stamp(&fixture, &fixture.row_a, "a1b2c3d4").is_err());
        assert!(!fixture.root.exists());
    }
}
