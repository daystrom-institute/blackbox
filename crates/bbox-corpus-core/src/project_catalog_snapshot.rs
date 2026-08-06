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
    /// The BUFFERED budget: how many bytes a whole-file-read owner may hold in
    /// memory across one capture. Applied cumulatively by
    /// [`capture_regular_tree_nofollow`], because for those owners it is an
    /// allocation ceiling, not a work ceiling.
    ///
    /// It does NOT apply to the streaming lane below. A line-oriented owner
    /// whose sources are legitimately multi-gigabyte (edge lanes) would refuse
    /// its own first file under a memory budget, which is exactly the failure
    /// the streaming lane exists to remove.
    pub max_source_bytes: usize,
    pub max_subsources: usize,
    pub max_rows: usize,
    pub max_selector_bytes: usize,
    /// The STREAMED budget: total bytes one streaming pass may read across the
    /// whole tree. Memory is O(chunk) regardless, so this bounds WALL TIME, not
    /// allocation, and is therefore set orders of magnitude above the buffered
    /// budget. It exists so a runaway or adversarial tree cannot make a
    /// migration preflight run unboundedly long; it is not a correctness bound.
    ///
    /// `u64` rather than `usize` because it is a quantity of work over a tree
    /// rather than the size of any allocation, and the default must be
    /// expressible on a 32-bit host.
    pub max_streamed_source_bytes: u64,
    /// Per-line ceiling for the streaming lane, the one bound that really is an
    /// allocation ceiling there: a streamed line is buffered whole so the owner
    /// can decode it. A single JSONL record above this is corruption, not data.
    pub max_streamed_line_bytes: usize,
}

impl Default for OwnerSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024 * 1024,
            max_subsources: 100_000,
            max_rows: 100_000,
            max_selector_bytes: 16 * 1024,
            // Generous on purpose: observed production edge-lane trees run to
            // several GiB with individual lanes above 1 GiB, and a preflight
            // that refuses a host for being BIG is the defect this replaces.
            max_streamed_source_bytes: 64 * 1024 * 1024 * 1024,
            max_streamed_line_bytes: 1024 * 1024,
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

/// Which owner rows one legacy-selector observation stands for.
///
/// Most owners are small stores where an observation IS one row, and the
/// singleton form says exactly that. Line-oriented owners are different in
/// kind: an edge-lane host carries millions of rows over a couple of hundred
/// distinct selectors, and a per-row ledger cannot fit the canonical inventory
/// at all. Those owners emit ONE observation per (subsource, selector) carrying
/// the same evidence the plan actually needs - how many rows it stands for, and
/// a canonical ordered commitment over their ids - so a dropped, duplicated, or
/// substituted member cannot hide behind an unchanged observation.
///
/// The commitment is over member ids in WALK ORDER, not sorted: the order is a
/// property of the source the owner just read, and re-deriving it is how a
/// verify proves it re-read the same rows rather than merely the same set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegacySelectorMembersV1 {
    pub row_count: u64,
    pub commitment_sha256: String,
}

const LEGACY_SELECTOR_MEMBERS_DOMAIN: &[u8] =
    b"blackbox.project-catalog.legacy-selector-members.v1";

/// Builds a [`LegacySelectorMembersV1`] one member at a time.
///
/// Incremental by construction: an owner aggregating millions of lane rows must
/// never hold their ids, only fold them.
#[derive(Debug, Clone)]
pub struct LegacySelectorMembersBuilderV1 {
    hasher: Sha256,
    row_count: u64,
}

impl Default for LegacySelectorMembersBuilderV1 {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacySelectorMembersBuilderV1 {
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, LEGACY_SELECTOR_MEMBERS_DOMAIN);
        Self {
            hasher,
            row_count: 0,
        }
    }

    pub fn push(&mut self, member_row_id: &str) {
        hash_field(&mut self.hasher, member_row_id.as_bytes());
        self.row_count = self.row_count.saturating_add(1);
    }

    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    pub fn finish(self) -> LegacySelectorMembersV1 {
        LegacySelectorMembersV1 {
            row_count: self.row_count,
            commitment_sha256: hex::encode(self.hasher.finalize()),
        }
    }
}

/// The member set of an observation that stands for exactly itself.
pub fn singleton_selector_members(stable_row_id: &str) -> LegacySelectorMembersV1 {
    let mut builder = LegacySelectorMembersBuilderV1::new();
    builder.push(stable_row_id);
    builder.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerSnapshotRowValueV1 {
    LegacyProjectSelector {
        selector_kind: LegacyProjectSelectorKindV1,
        literal_selector: String,
        members: LegacySelectorMembersV1,
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
    /// An observation that stands for exactly ONE owner row: itself.
    ///
    /// The shape every small store uses. An owner whose rows outnumber what a
    /// canonical inventory can hold aggregates instead, through
    /// [`Self::legacy_selector_aggregate`].
    pub fn legacy_selector(
        stable_row_id: impl Into<String>,
        selector_kind: LegacyProjectSelectorKindV1,
        literal_selector: impl Into<String>,
    ) -> Self {
        let stable_row_id = stable_row_id.into();
        let members = singleton_selector_members(&stable_row_id);
        Self::legacy_selector_aggregate(stable_row_id, selector_kind, literal_selector, members)
    }

    /// An observation standing for a SET of owner rows that share one selector.
    ///
    /// The observation id is the owner's name for the set, not for any member,
    /// and the apply side resolves it by re-walking the source rather than by
    /// looking up one row.
    pub fn legacy_selector_aggregate(
        stable_row_id: impl Into<String>,
        selector_kind: LegacyProjectSelectorKindV1,
        literal_selector: impl Into<String>,
        members: LegacySelectorMembersV1,
    ) -> Self {
        Self {
            stable_row_id: stable_row_id.into(),
            value: OwnerSnapshotRowValueV1::LegacyProjectSelector {
                selector_kind,
                literal_selector: literal_selector.into(),
                members,
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
/// The rows this obligation stands for are no longer the rows it was planned
/// against: one was removed, duplicated, or substituted since capture.
///
/// STALENESS class, deliberately alongside [`OWNER_SOURCE_MOVED`] rather than
/// alongside the invalidity codes. Nothing here says the owner is corrupt; it
/// says the artifact describes a state the store has since left, and the
/// operator response is the same one a moved source calls for: re-run preflight
/// against the current state.
///
/// Capture records a member count and an ordered commitment precisely so a
/// dropped, duplicated, or substituted member is detectable. That evidence is
/// worth nothing unless something REDERIVES it at the moment of writing, which
/// is what this code exists to report: a group whose members changed while
/// remaining uniformly stamped would otherwise pass verification unchanged.
pub const OWNER_ROW_MEMBERS_MOVED: &str = "owner_row_members_moved";

/// Refuse unless the member evidence a plan was built from still describes the
/// rows the owner just walked.
pub fn ensure_selector_members_unchanged(
    expected: &LegacySelectorMembersV1,
    observed: &LegacySelectorMembersV1,
) -> Result<(), OwnerRowStampError> {
    if expected == observed {
        return Ok(());
    }
    Err(OwnerRowStampError::new(OWNER_ROW_MEMBERS_MOVED))
}

/// The singleton-owner form of [`ensure_selector_members_unchanged`].
///
/// An owner whose observation IS one row has a member set that is a pure
/// function of the row id, so "rederive the member set" is exactly "the plan
/// expected this one row, and the lookup that follows proves it is still
/// there". Shared by all twelve singleton owners so the rule has one
/// definition rather than twelve.
pub fn ensure_singleton_member_evidence(
    source_row_id: &str,
    expected: &LegacySelectorMembersV1,
) -> Result<(), OwnerRowStampError> {
    ensure_selector_members_unchanged(expected, &singleton_selector_members(source_row_id))
}

/// [`ensure_singleton_member_evidence`] for a batched read.
pub fn ensure_singleton_member_evidence_batch(
    rows: &std::collections::BTreeMap<String, LegacySelectorMembersV1>,
) -> Result<(), OwnerRowStampError> {
    for (source_row_id, expected) in rows {
        ensure_singleton_member_evidence(source_row_id, expected)?;
    }
    Ok(())
}

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

// ---------------------------------------------------------------------------
// Row reading: the verify half of the owner snapshot contract
// ---------------------------------------------------------------------------
//
// Capture answers "which rows still carry a legacy path selector" and stamping
// writes the stable id onto one of them. Neither answers the question the
// backfill's VERIFY has to answer: does the row an applied plan claims to have
// stamped actually exist, and does it actually carry that exact project id?
//
// Nothing already here can answer it. Capture emits selector rows and never
// reports a row's `project_id`, so a stamped row and an unstamped one look
// identical to it; and the stamp path answers only by WRITING, which a verify
// must not do. These helpers are the third face of the same contract, sharing
// the owners' row identity, their nofollow reads, and their error vocabulary.

/// What one durable owner row carries. An ABSENT row is not a variant: it is
/// `Err(OWNER_ROW_ABSENT)`, exactly as on the stamping side, because
/// absence-as-success is how a partial backfill would report itself complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerRowProjectIdV1 {
    /// The row carries this stable project id.
    Stamped(String),
    /// The row exists and carries no stable project id.
    Unstamped,
}

/// What one owner answered about the rows a verify asked it for.
///
/// An id ABSENT from this map is the owner saying "I hold no such row", which is
/// the batched form of the single-row `OWNER_ROW_ABSENT` refusal. The map is
/// keyed by the caller's own `source_row_id` strings, so a caller never has to
/// re-derive row identity to read an answer back.
pub type OwnerRowBatchV1 = std::collections::BTreeMap<String, OwnerRowProjectIdV1>;

/// What a batched read ASKS an owner: which rows, and the member evidence each
/// obligation was planned against.
///
/// The evidence travels with the request rather than being looked up later
/// because only the owner can rederive it, and it must be rederived from the
/// same walk that answers the read. A verify that compared counts afterwards
/// would be comparing two different reads of a live store.
pub type OwnerRowRequestV1 = std::collections::BTreeMap<String, LegacySelectorMembersV1>;

// ---------------------------------------------------------------------------
// Capture instrumentation
// ---------------------------------------------------------------------------
//
// The read half's cost contract - ONE lock-and-decode, or ONE tree walk, per
// owner no matter how many rows are asked for - is not visible in a result
// value: a reader that re-captured per row would return exactly the same
// answers, only slowly and from as many different durable snapshots as it made
// captures. That is what makes it a defect a test cannot otherwise see, so the
// captures are counted and the count is assertable.
//
// The counter is THREAD-LOCAL, not global. Every owner read runs inline on its
// caller's thread, so a thread-local count is exact for that caller and cannot
// be perturbed by a parallel test in the same process - which a global counter
// would be, silently, under any harness that shares one.
thread_local! {
    static OWNER_ROW_READ_CAPTURES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record one owner-source capture: a locked whole-file read, or one tree walk.
///
/// Owners implementing their own walk (the transcript-edge lane set) call this
/// themselves; the shared helpers below call it for everyone else.
pub fn note_owner_row_read_capture() {
    OWNER_ROW_READ_CAPTURES.with(|count| count.set(count.get().saturating_add(1)));
}

/// Owner-source captures performed by the row READ path on this thread.
pub fn owner_row_read_captures() -> u64 {
    OWNER_ROW_READ_CAPTURES.with(std::cell::Cell::get)
}

/// Zero the capture count, so a test can measure one owner read in isolation.
pub fn reset_owner_row_read_captures() {
    OWNER_ROW_READ_CAPTURES.with(|count| count.set(0));
}

/// Read MANY rows of a JSON owner source, mirroring [`stamp_json_owner_row`].
///
/// One lock, one capture, one decode, all requested ids resolved from that
/// single snapshot. Batched rather than per-row for two reasons, and the second
/// is the load-bearing one:
///
/// 1. Per-row reads make verifying an owner quadratic - O(rows x source bytes) -
///    which on a large store can extend or abort the stopped-service closeout
///    window a backfill verify runs inside.
/// 2. Separate lock-and-capture cycles observe separate durable snapshots. A
///    per-row reader can therefore return one owner's answers assembled from
///    several different states of that owner, and report a combination that
///    never existed. A batch has ONE state by construction.
///
/// The exclusive store lock is held for the read for the same reason the write
/// half holds it: a verify that raced a concurrent writer could observe a row
/// mid-replacement and report a defect that never existed.
pub fn read_json_owner_rows(
    store_path: &Path,
    source_id: &str,
    subsource_id: &str,
    limits: OwnerSnapshotLimitsV1,
    locate: impl FnOnce(&[u8]) -> Result<OwnerRowBatchV1, OwnerRowStampError>,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    validate_limits(limits).map_err(|error| OwnerRowStampError::new(error.code))?;
    crate::json_store::with_store_lock(store_path, || {
        note_owner_row_read_capture();
        let captured = capture_regular_file_nofollow(
            store_path,
            source_id,
            subsource_id,
            limits.max_source_bytes,
        );
        let Some(bytes) = captured.bytes else {
            let code = match captured.state {
                OwnerSnapshotStateV1::Missing { .. } => OWNER_SOURCE_MISSING,
                _ => OWNER_SOURCE_UNREADABLE,
            };
            return Ok(Err(OwnerRowStampError::new(code)));
        };
        Ok(locate(&bytes))
    })
    .unwrap_or_else(|_| Err(OwnerRowStampError::new(OWNER_SOURCE_UNREADABLE)))
}

/// Read one row object's stable project id.
///
/// Deliberately the exact inverse of [`stamp_row_object`], down to the
/// treatment of a blank string as unstamped (which is what
/// [`decide_row_stamp`] does) and of a non-string value as a CORRUPT row rather
/// than an unstamped one.
pub fn read_row_object_project_id(
    row: &serde_json::Value,
) -> Result<OwnerRowProjectIdV1, OwnerRowStampError> {
    let object = row
        .as_object()
        .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_INVALID))?;
    match object.get(OWNER_ROW_PROJECT_ID_FIELD) {
        None | Some(serde_json::Value::Null) => Ok(OwnerRowProjectIdV1::Unstamped),
        Some(serde_json::Value::String(existing)) if existing.trim().is_empty() => {
            Ok(OwnerRowProjectIdV1::Unstamped)
        }
        Some(serde_json::Value::String(existing)) => {
            Ok(OwnerRowProjectIdV1::Stamped(existing.clone()))
        }
        Some(_) => Err(OwnerRowStampError::new(OWNER_SOURCE_INVALID)),
    }
}

/// Collect the requested rows of a decoded row sequence into a batch.
///
/// Shared by the array, map, and top-level-array shapes so all three agree on
/// what a batch answer means: an id present in the map was found, an id absent
/// was not held by the owner, and a CORRUPT row fails the whole batch rather
/// than reading as unstamped.
fn collect_requested_rows<'a>(
    rows: impl Iterator<Item = &'a serde_json::Value>,
    source_row_ids: &BTreeSet<String>,
    row_id_of: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    let mut batch = OwnerRowBatchV1::new();
    for row in rows {
        let Some(row_id) = row_id_of(row) else {
            continue;
        };
        // FIRST match wins, exactly as the stamping side's `find` does: the two
        // halves of the contract must resolve one id to the same row even in a
        // source that duplicates it.
        if !source_row_ids.contains(&row_id) || batch.contains_key(&row_id) {
            continue;
        }
        batch.insert(row_id, read_row_object_project_id(row)?);
    }
    Ok(batch)
}

/// Read half of [`stamp_json_array_row`], for MANY rows of one decoded source.
pub fn read_json_array_rows_project_id(
    bytes: &[u8],
    array_field: &str,
    id_field: &str,
    source_row_ids: &BTreeSet<String>,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    let document: serde_json::Value = decode_owner_source(bytes)?;
    let Some(rows) = document
        .get(array_field)
        .and_then(serde_json::Value::as_array)
    else {
        // A source holding no such array holds none of the requested rows. The
        // caller reports that as an absent row, which is what it is.
        return Ok(OwnerRowBatchV1::new());
    };
    collect_requested_rows(rows.iter(), source_row_ids, |row| {
        row.get(id_field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    })
}

/// Read half of [`stamp_json_map_row`], for MANY rows of one decoded source.
pub fn read_json_map_rows_project_id(
    bytes: &[u8],
    map_field: &str,
    source_row_ids: &BTreeSet<String>,
    row_id_of: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    let document: serde_json::Value = decode_owner_source(bytes)?;
    let Some(rows) = document
        .get(map_field)
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(OwnerRowBatchV1::new());
    };
    collect_requested_rows(rows.values(), source_row_ids, row_id_of)
}

/// Read half of [`stamp_json_tree_row`], locating the records the same way.
///
/// ONE walk of the tree answers every requested id. The walk is the expensive
/// half of a tree owner's read, and the whole point of the batch: a per-row
/// caller walked the entire tree once per row.
pub fn read_json_tree_rows_project_id(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool + Copy,
    row_id_of: impl Fn(&str, &serde_json::Value) -> Option<String>,
    source_row_ids: &BTreeSet<String>,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    note_owner_row_read_capture();
    let captures = capture_stable_regular_tree_nofollow(root, source_id, limits, include)
        .map_err(|error| OwnerRowStampError::new(error.code))?;
    let mut batch = OwnerRowBatchV1::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id(source_id, &relative);
        let Some(bytes) = captured.bytes else {
            continue;
        };
        let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(row_id) = row_id_of(&subsource_id, &document) else {
            continue;
        };
        if !source_row_ids.contains(&row_id) || batch.contains_key(&row_id) {
            continue;
        }
        batch.insert(row_id, read_row_object_project_id(&document)?);
    }
    Ok(batch)
}

/// Read half of [`stamp_legacy_task_owner_row`]. `tasks.json` is a top-level
/// array, so it locates its rows the same non-shared way the stamper does.
pub fn read_legacy_task_owner_rows(
    tasks_path: &Path,
    rows: &OwnerRowRequestV1,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows.keys().cloned().collect::<BTreeSet<_>>();
    read_json_owner_rows(tasks_path, "task", "task:central-json", limits, |bytes| {
        let document: serde_json::Value = decode_owner_source(bytes)?;
        let rows = document
            .as_array()
            .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_INVALID))?;
        collect_requested_rows(rows.iter(), source_row_ids, |row| {
            row.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
    })
}

/// Read half of [`stamp_legacy_proposal_owner_row`].
pub fn read_legacy_proposal_owner_rows(
    proposals_root: &Path,
    rows: &OwnerRowRequestV1,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowBatchV1, OwnerRowStampError> {
    ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows.keys().cloned().collect::<BTreeSet<_>>();
    read_json_tree_rows_project_id(
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
        source_row_ids,
    )
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
    expected_members: &LegacySelectorMembersV1,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    ensure_singleton_member_evidence(source_row_id, expected_members)?;
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
    expected_members: &LegacySelectorMembersV1,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    ensure_singleton_member_evidence(source_row_id, expected_members)?;
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
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    for relative in enumerate_regular_tree_nofollow(root, limits, include)? {
        let subsource_id = stable_subsource_id(source_id, &relative);
        let captured = capture_regular_file_nofollow(
            &root.join(&relative),
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
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

// ---------------------------------------------------------------------------
// The streaming lane: owners whose sources do not fit in memory
// ---------------------------------------------------------------------------
//
// [`capture_regular_tree_nofollow`] reads each source whole and spends one
// shared byte budget across the tree. That is right for the small JSON stores:
// their fingerprint is over bytes they must hold anyway, and a cumulative cap
// is a real memory ceiling.
//
// It is wrong for a LINE-ORIENTED owner. Edge lanes on a working host run to
// several GiB with single lanes above 1 GiB, so the first file alone exhausts a
// memory-shaped budget, comes back with no bytes, and the owner reports the host
// as corrupt (`owner_source_unreadable`) purely for being large. The lane below
// reads the same trees with the same safety rules, but digests incrementally and
// hands the owner one complete line at a time, so memory is O(chunk + line) no
// matter how big the source is, and the budget that remains is a WALL-TIME bound
// rather than an allocation one.

/// One file of a streamed owner-tree capture: the same `Present`/`Missing`/
/// `Corrupt` state the buffered lane produces, plus the rows the owner decoded
/// from it. The raw bytes are deliberately absent - not holding them is the
/// entire point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedOwnerFileV1 {
    pub relative: PathBuf,
    pub subsource_id: String,
    pub state: OwnerSnapshotStateV1,
    pub rows: Vec<OwnerSnapshotRowV1>,
}

/// A refusal from the streaming capture, carrying the subsource it is ABOUT.
///
/// The buffered lane's [`OwnerSnapshotError`] can only say "the tree is bad",
/// which is all a whole-tree read needs. A streaming decoder refuses on one
/// specific line of one specific file, and an owner reporting corruption must
/// name it, so the subsource travels with the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedOwnerTreeErrorV1 {
    pub code: &'static str,
    /// `None` when the refusal is about the tree rather than any one file.
    pub subsource_id: Option<String>,
}

impl From<OwnerSnapshotError> for StreamedOwnerTreeErrorV1 {
    fn from(error: OwnerSnapshotError) -> Self {
        Self {
            code: error.code,
            subsource_id: None,
        }
    }
}

impl std::fmt::Display for StreamedOwnerTreeErrorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for StreamedOwnerTreeErrorV1 {}

/// How many bytes one read syscall takes. The streaming lane's resident set is
/// this plus the longest line it has seen, and nothing else.
pub const STREAMED_CHUNK_BYTES: usize = 64 * 1024;

/// Splits streamed chunks into complete lines, preserving each line's EXACT
/// terminator.
///
/// The one definition of "where do this owner's lines begin and end", shared by
/// the capture that reads a lane and the stamp that rewrites it. They must
/// agree: capture numbers rows by line and the stamp copies every line it did
/// not change through byte for byte, so a splitter that disagreed with itself
/// would either renumber rows or corrupt the file.
///
/// Reproduces `split_inclusive('\n')` exactly: a trailing terminator does not
/// mint an empty final line, an unterminated tail IS a line, and `\r` belongs
/// to the terminator only when a `\n` actually followed it.
pub struct StreamedLineSplitterV1 {
    line: Vec<u8>,
    max_line_bytes: usize,
}

impl StreamedLineSplitterV1 {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            max_line_bytes,
        }
    }

    /// Feed one chunk, calling `on_line(content, terminator)` for every line the
    /// chunk completes.
    pub fn push_chunk(
        &mut self,
        chunk: &[u8],
        mut on_line: impl FnMut(&[u8], &[u8]) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let mut rest = chunk;
        while let Some(position) = rest.iter().position(|byte| *byte == b'\n') {
            self.line.extend_from_slice(&rest[..position]);
            self.check_bound()?;
            let (content, terminator) = if self.line.last() == Some(&b'\r') {
                (&self.line[..self.line.len() - 1], &b"\r\n"[..])
            } else {
                (&self.line[..], &b"\n"[..])
            };
            on_line(content, terminator)?;
            self.line.clear();
            rest = &rest[position + 1..];
        }
        self.line.extend_from_slice(rest);
        // Checked every chunk, not only at a terminator, so an unterminated
        // multi-gigabyte "line" is refused instead of buffered.
        self.check_bound()
    }

    /// Emit the unterminated tail, if the source had one.
    pub fn finish(
        self,
        on_line: impl FnOnce(&[u8], &[u8]) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        if self.line.is_empty() {
            return Ok(());
        }
        on_line(&self.line, b"")
    }

    fn check_bound(&self) -> Result<(), &'static str> {
        if self.line.len() > self.max_line_bytes {
            return Err("owner_source_line_limit");
        }
        Ok(())
    }
}

/// Capture a line-oriented owner tree by streaming, with the same two-identical-
/// scans stability contract [`capture_stable_regular_tree_nofollow`] gives the
/// buffered owners.
///
/// The stability comparison is over per-file CONTENT DIGESTS rather than over
/// bytes. That is the same guarantee at a fraction of the memory: rows are a
/// pure function of the bytes, so two passes that agree on every file's sha256
/// necessarily agree on every row. The first pass therefore decodes nothing at
/// all - it only digests - and the rows come from the pass that has to agree
/// with it.
///
/// `new_lane` builds the owner's per-file decoding state, `decode_line`
/// advances it, and `finish_lane` turns the finished state into that file's
/// rows. A FRESH state is built for every file in every pass, so a pass
/// abandoned mid-tree can never leak occurrence counters or partial rows into
/// the retry that replaces it.
///
/// Rows come out of `finish_lane` rather than out of `decode_line` because an
/// owner that AGGREGATES cannot know a row until it has seen the whole file: a
/// per-line emitter would force it to either buffer members or emit a row it
/// then has to revise.
pub fn capture_stable_streamed_tree_nofollow<D>(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool + Copy,
    new_lane: impl Fn(&str) -> D + Copy,
    decode_line: impl Fn(&mut D, &[u8]) -> Result<(), &'static str> + Copy,
    finish_lane: impl Fn(D) -> Result<Vec<OwnerSnapshotRowV1>, &'static str> + Copy,
) -> Result<Vec<StreamedOwnerFileV1>, StreamedOwnerTreeErrorV1> {
    validate_limits(limits)?;
    let authority = crate::json_store::NofollowDirectory::open_existing(root)
        .map_err(|_| OwnerSnapshotError {
            code: "owner_tree_unsafe",
        })?
        .ok_or(OwnerSnapshotError {
            code: "owner_tree_changed_during_capture",
        })?;
    // Digest-only, so the cheap pass is the one that may have to be discarded.
    let mut prior = stream_regular_tree_nofollow(
        root,
        source_id,
        limits,
        include,
        |_subsource_id: &str| (),
        |_lane: &mut (), _line: &[u8]| -> Result<(), &'static str> { Ok(()) },
        |_lane: ()| -> Result<Vec<OwnerSnapshotRowV1>, &'static str> { Ok(Vec::new()) },
    )?;
    for _ in 0..3 {
        let current = stream_regular_tree_nofollow(
            root,
            source_id,
            limits,
            include,
            new_lane,
            decode_line,
            finish_lane,
        )?;
        if streamed_states_agree(&prior, &current) {
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
    }
    .into())
}

/// Two passes agree when they saw the same files in the same states. Rows are
/// excluded deliberately: they are derived from bytes the digests already cover,
/// and comparing them would make the stability check quadratic in row count for
/// no additional guarantee.
fn streamed_states_agree(left: &[StreamedOwnerFileV1], right: &[StreamedOwnerFileV1]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.relative == right.relative
                && left.subsource_id == right.subsource_id
                && left.state == right.state
        })
}

/// One streaming pass over the tree.
fn stream_regular_tree_nofollow<D>(
    root: &Path,
    source_id: &str,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool,
    new_lane: impl Fn(&str) -> D,
    decode_line: impl Fn(&mut D, &[u8]) -> Result<(), &'static str>,
    finish_lane: impl Fn(D) -> Result<Vec<OwnerSnapshotRowV1>, &'static str>,
) -> Result<Vec<StreamedOwnerFileV1>, StreamedOwnerTreeErrorV1> {
    // Per PASS, never across passes: a retried pass re-reads the same tree and
    // must be allowed the same work as the pass it replaces.
    let mut budget = limits.max_streamed_source_bytes;
    let mut rows_seen = 0usize;
    let mut files = Vec::new();
    for relative in enumerate_regular_tree_nofollow(root, limits, include)? {
        let subsource_id = stable_subsource_id(source_id, &relative);
        let mut lane = new_lane(subsource_id.as_str());
        let fail = |code: &'static str| StreamedOwnerTreeErrorV1 {
            code,
            subsource_id: Some(subsource_id.clone()),
        };
        let state = stream_regular_file_nofollow(
            &root.join(&relative),
            source_id,
            &subsource_id,
            limits,
            &mut budget,
            |line| decode_line(&mut lane, line),
        )
        .map_err(fail)?;
        let rows = finish_lane(lane).map_err(fail)?;
        rows_seen = rows_seen.saturating_add(rows.len());
        // Enforced HERE rather than only in `build_owner_snapshot` so a tree
        // whose rows cannot fit is refused as it is walked, not after the whole
        // set has been accumulated.
        if rows_seen > limits.max_rows {
            return Err(fail("owner_row_limit"));
        }
        files.push(StreamedOwnerFileV1 {
            relative,
            subsource_id,
            state,
            rows,
        });
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

/// Stream one regular file: incremental sha256 over the raw bytes, plus every
/// complete line handed to `on_line`.
///
/// The state vocabulary is identical to [`capture_regular_file_nofollow`]'s, so
/// an owner that switches lanes keeps its diagnostics: an absent file is
/// `Missing`, an unsafe parent is `owner_parent_unsafe`, and anything that
/// cannot be opened or read - a non-regular entry, a permission refusal, an I/O
/// failure - is `owner_source_unreadable`. `Err` is reserved for the bounds that
/// abandon the whole capture rather than describing one file.
///
/// Line splitting reproduces `split_inclusive('\n')` exactly: a trailing
/// terminator does not mint an empty final line, an unterminated tail IS a line,
/// and a `\r` is trimmed only when a `\n` actually terminated the line. Owners
/// index rows by line, so the streamed walk and any whole-body walk of the same
/// file must agree line for line.
fn stream_regular_file_nofollow(
    path: &Path,
    source_id: &str,
    subsource_id: &str,
    limits: OwnerSnapshotLimitsV1,
    budget: &mut u64,
    mut on_line: impl FnMut(&[u8]) -> Result<(), &'static str>,
) -> Result<OwnerSnapshotStateV1, &'static str> {
    use std::io::Read as _;

    let missing = || OwnerSnapshotStateV1::Missing {
        fingerprint: state_fingerprint("missing", source_id, subsource_id),
    };
    let corrupt = |diagnostic_code: &str| OwnerSnapshotStateV1::Corrupt {
        diagnostic_code: diagnostic_code.to_string(),
        fingerprint: state_fingerprint(diagnostic_code, source_id, subsource_id),
    };
    let Some(parent) = path.parent() else {
        return Ok(corrupt("owner_path_has_no_parent"));
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(corrupt("owner_filename_invalid"));
    };
    let directory = match crate::json_store::NofollowDirectory::open_existing(parent) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Ok(missing()),
        Err(_) => return Ok(corrupt("owner_parent_unsafe")),
    };
    let mut file = match directory.open_regular(name, "owner source") {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(missing()),
        Err(_) => return Ok(corrupt("owner_source_unreadable")),
    };

    let mut hasher = Sha256::new();
    let mut byte_len = 0u64;
    let mut chunk = vec![0u8; STREAMED_CHUNK_BYTES];
    let mut splitter = StreamedLineSplitterV1::new(limits.max_streamed_line_bytes);
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            // A signal is not corruption. `read_to_end` retries this for the
            // buffered lane; a raw `read` has to do it itself, and a capture
            // that streams gigabytes gets far more chances to be interrupted.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(corrupt("owner_source_unreadable")),
        };
        *budget = budget
            .checked_sub(read as u64)
            .ok_or("owner_source_byte_limit")?;
        hasher.update(&chunk[..read]);
        byte_len += read as u64;
        splitter.push_chunk(&chunk[..read], |content, _terminator| on_line(content))?;
    }
    splitter.finish(|content, _terminator| on_line(content))?;
    Ok(OwnerSnapshotStateV1::Present {
        content_sha256: hex::encode(hasher.finalize()),
        byte_len,
    })
}

/// The tree RULES, with no reads: which entries an owner tree admits, and what
/// makes it unsafe.
///
/// Split out so the buffered and streaming captures cannot disagree about the
/// shape of a legal owner tree (no symlink at any depth, no non-UTF-8 or
/// non-normal component, directories recursed, everything else ignored).
/// Deliberately returns VISIT order rather than sorted order: the buffered
/// caller applies a cumulative byte budget while it reads, so the order it sees
/// files in is part of its observable behavior.
///
/// Public because the apply half of a streaming owner needs the tree rules
/// without the reads: it locates the ONE file an obligation names and must
/// never pay for reading the rest.
pub fn enumerate_regular_tree_nofollow(
    root: &Path,
    limits: OwnerSnapshotLimitsV1,
    include: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, OwnerSnapshotError> {
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
    let mut files: Vec<PathBuf> = Vec::new();
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
            files.push(relative);
        }
    }
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
            literal_selector,
            members,
            ..
        } = &row.value
        {
            if literal_selector.is_empty()
                || literal_selector.len() > limits.max_selector_bytes
                || literal_selector
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
            {
                return Err(OwnerSnapshotError {
                    code: "owner_selector_invalid",
                });
            }
            // An observation standing for NO rows is not evidence of anything.
            // Refusing it here is what stops an aggregating owner from
            // reporting an empty selector group as a stamping obligation.
            if members.row_count == 0 || !valid_sha256(&members.commitment_sha256) {
                return Err(OwnerSnapshotError {
                    code: "owner_selector_members_invalid",
                });
            }
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
        || limits.max_streamed_source_bytes == 0
        || limits.max_streamed_line_bytes == 0
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
            members,
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
            // The member set is EVIDENCE, so it is committed: an aggregate
            // whose membership changed must not present an unchanged row
            // commitment to a verify that only compares hashes.
            hash_field(hasher, &members.row_count.to_be_bytes());
            hash_field(hasher, members.commitment_sha256.as_bytes());
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
mod streamed_owner_tree {
    use super::*;

    /// A decoder that turns every line into a row, so a test can read back the
    /// exact line sequence the walk produced.
    struct LineProbe {
        subsource_id: String,
        rows: Vec<OwnerSnapshotRowV1>,
    }

    fn new_probe(subsource_id: &str) -> LineProbe {
        LineProbe {
            subsource_id: subsource_id.to_string(),
            rows: Vec::new(),
        }
    }

    fn probe_line(probe: &mut LineProbe, line: &[u8]) -> Result<(), &'static str> {
        let text = std::str::from_utf8(line).map_err(|_| "probe_line_invalid")?;
        probe.rows.push(OwnerSnapshotRowV1::legacy_selector(
            format!("{}:{}", probe.subsource_id, probe.rows.len()),
            LegacyProjectSelectorKindV1::AbsolutePath,
            // Bracketed so an empty line is still a legible, non-empty literal.
            format!("[{text}]"),
        ));
        Ok(())
    }

    fn finish_probe(probe: LineProbe) -> Result<Vec<OwnerSnapshotRowV1>, &'static str> {
        Ok(probe.rows)
    }

    fn capture(
        root: &Path,
        limits: OwnerSnapshotLimitsV1,
    ) -> Result<Vec<StreamedOwnerFileV1>, StreamedOwnerTreeErrorV1> {
        capture_stable_streamed_tree_nofollow(
            root,
            "probe",
            limits,
            |relative| relative.extension().and_then(|ext| ext.to_str()) == Some("jsonl"),
            new_probe,
            probe_line,
            finish_probe,
        )
    }

    fn selectors(file: &StreamedOwnerFileV1) -> Vec<String> {
        file.rows
            .iter()
            .map(|row| match &row.value {
                OwnerSnapshotRowValueV1::LegacyProjectSelector {
                    literal_selector, ..
                } => literal_selector.clone(),
                OwnerSnapshotRowValueV1::InventoryTarget { .. } => unreachable!("probe rows"),
            })
            .collect()
    }

    /// The streamed walk must see EXACTLY the lines a `split_inclusive('\n')`
    /// walk of the same bytes sees. Owners index rows by line position, so a
    /// walk that invented or dropped a line would mint row ids no other half of
    /// the backfill could reproduce.
    #[test]
    fn streamed_lines_reproduce_split_inclusive_with_crlf_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // A CRLF line, a bare-LF line, an empty line, and an UNTERMINATED tail.
        std::fs::write(root.join("lane.jsonl"), b"a\r\nb\n\nc").unwrap();

        let captured = capture(&root, OwnerSnapshotLimitsV1::default()).unwrap();
        assert_eq!(selectors(&captured[0]), vec!["[a]", "[b]", "[]", "[c]"]);

        // A trailing terminator does NOT mint an empty final line, and an empty
        // file has no lines at all.
        std::fs::write(root.join("lane.jsonl"), b"a\n").unwrap();
        let captured = capture(&root, OwnerSnapshotLimitsV1::default()).unwrap();
        assert_eq!(selectors(&captured[0]), vec!["[a]"]);

        std::fs::write(root.join("lane.jsonl"), b"").unwrap();
        let captured = capture(&root, OwnerSnapshotLimitsV1::default()).unwrap();
        assert!(captured[0].rows.is_empty());
    }

    /// The incremental digest is the same commitment a whole-file read would
    /// have produced. Without this, moving an owner onto the streaming lane
    /// would silently re-fingerprint its entire corpus.
    #[test]
    fn the_incremental_digest_equals_the_whole_file_digest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("lane.jsonl");
        let body = (0..5_000)
            .map(|index| format!("line-{index}\n"))
            .collect::<String>();
        std::fs::write(&path, &body).unwrap();

        // A per-file buffered budget far below the file size: the streaming
        // lane must not consult it at all.
        let limits = OwnerSnapshotLimitsV1 {
            max_source_bytes: 1024,
            ..OwnerSnapshotLimitsV1::default()
        };
        let captured = capture(&root, limits).unwrap();
        assert_eq!(
            captured[0].state,
            OwnerSnapshotStateV1::Present {
                content_sha256: sha256_hex(body.as_bytes()),
                byte_len: body.len() as u64,
            }
        );
        assert_eq!(captured[0].rows.len(), 5_000);
    }

    /// The streamed budget bounds WORK, and names the file it ran out on.
    #[test]
    fn the_streamed_byte_budget_refuses_and_names_its_subsource() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lane.jsonl"), "x".repeat(4096)).unwrap();

        let limits = OwnerSnapshotLimitsV1 {
            max_streamed_source_bytes: 64,
            ..OwnerSnapshotLimitsV1::default()
        };
        let error = capture(&root, limits).unwrap_err();
        assert_eq!(error.code, "owner_source_byte_limit");
        assert_eq!(
            error.subsource_id.as_deref(),
            Some(stable_subsource_id("probe", Path::new("lane.jsonl")).as_str())
        );
    }

    /// One unterminated line cannot become an unbounded buffer: the per-line
    /// ceiling is the streaming lane's real allocation bound.
    #[test]
    fn an_overlong_line_refuses_instead_of_buffering() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lane.jsonl"), "x".repeat(4096)).unwrap();

        let limits = OwnerSnapshotLimitsV1 {
            max_streamed_line_bytes: 64,
            ..OwnerSnapshotLimitsV1::default()
        };
        assert_eq!(
            capture(&root, limits).unwrap_err().code,
            "owner_source_line_limit"
        );
    }

    /// The member commitment is ORDERED and count-bearing: a set that lost a
    /// member, gained one, or saw the same members in another order must not
    /// present the same evidence.
    #[test]
    fn the_member_commitment_moves_with_membership_and_order() {
        let of = |members: &[&str]| {
            let mut builder = LegacySelectorMembersBuilderV1::new();
            for member in members {
                builder.push(member);
            }
            builder.finish()
        };

        let base = of(&["a", "b", "c"]);
        assert_eq!(base.row_count, 3);
        assert_eq!(of(&["a", "b", "c"]), base);
        assert_ne!(of(&["a", "c", "b"]), base);
        assert_ne!(of(&["a", "b"]), base);
        assert_ne!(of(&["a", "b", "c", "c"]), base);
        // Concatenation cannot be forged across a member boundary: the ids are
        // length-prefixed, so "ab" + "c" is not "a" + "bc".
        assert_ne!(of(&["ab", "c"]), of(&["a", "bc"]));
        assert_eq!(singleton_selector_members("a"), of(&["a"]));
    }

    /// Rows are counted as they stream, so a pathological tree is refused
    /// BEFORE its rows are accumulated rather than after.
    #[test]
    fn the_row_ceiling_refuses_while_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lane.jsonl"), "a\nb\nc\nd\n").unwrap();

        let limits = OwnerSnapshotLimitsV1 {
            max_rows: 2,
            ..OwnerSnapshotLimitsV1::default()
        };
        assert_eq!(capture(&root, limits).unwrap_err().code, "owner_row_limit");
    }
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
                ..
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
            &singleton_selector_members(row),
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
