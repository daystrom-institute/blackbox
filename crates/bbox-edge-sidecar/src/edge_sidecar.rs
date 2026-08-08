//! Edge sidecar persistence layer: the on-disk JSONL edge lanes
//! (observed / explicit / managed-derived), dir layout helpers, dedup
//! append/replace/merge/purge primitives, and legacy-sidecar compaction.
//! Store-agnostic by design — the store->edge emitters live in
//! `edge_index`.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_corpus_core::project_catalog_snapshot::{
    LegacyProjectSelectorKindV1, LegacySelectorMembersBuilderV1, LegacySelectorMembersV1,
    OWNER_PROJECT_ID_INVALID, OWNER_ROW_ABSENT, OWNER_SOURCE_MISSING, OWNER_SOURCE_MOVED,
    OWNER_SOURCE_UNREADABLE, OWNER_SOURCE_UNWRITABLE, OwnerRowProjectIdV1, OwnerRowStampError,
    OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1, OwnerSnapshotRowV1, RowStampDecisionV1,
    STREAMED_CHUNK_BYTES, StreamedLineSplitterV1, ensure_selector_members_unchanged,
    read_row_object_project_id, stamp_row_object,
};

static COMPACTION_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub provenance: EdgeProvenance,
    pub confidence: EdgeConfidence,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// The durable project this row belongs to, stamped by the Phase 6
    /// catalog backfill (plan section 3.3, adjudication Q-E1).
    ///
    /// A TYPED top-level field rather than a `metadata` key: project ownership
    /// is authority, and burying it in the free-form string map would make it
    /// indistinguishable from the incidental annotations already living there
    /// (including the `cwd` this stamp is meant to supersede).
    ///
    /// `skip_serializing_if` keeps an unstamped edge byte-identical to what
    /// every pre-Phase-6 writer produced, so adding this field does not rewrite
    /// the corpus and does not disturb [`transcript_edge_row_identity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeKey {
    source: EntityRef,
    kind: String,
    target: EntityRef,
    provenance: EdgeProvenance,
    confidence: EdgeConfidence,
}

/// Capture edge rows whose metadata retains a literal execution directory.
///
/// Every JSONL file is read by an exact no-follow file descriptor and the
/// complete tree is accepted only after two identical scans. This makes the
/// read-only capture coherent with atomic lane replacement without creating
/// an edge store or coordination file.
///
/// STREAMED, unlike every other durable owner. Edge lanes are the one owner
/// whose sources are unbounded by design: a working host carries several GiB of
/// them with individual lanes above 1 GiB, so a whole-file read would refuse the
/// host for being large (`owner_source_unreadable`) long before it found
/// anything wrong with it. Digesting incrementally and decoding line by line
/// keeps this capture's memory independent of lane size, and leaves
/// `owner_source_unreadable` to mean what it says.
/// The DURABLE lane predicate, shared by capture and the stamper's lane
/// enumeration so the two halves cannot disagree about the owner's
/// population.
///
/// An explicit ALLOW-list of the live lane layouts the edge store writes:
/// the top-level legacy combined lane `<project>.jsonl` and the split lanes
/// `explicit/<project>.jsonl` / `observed/<project>.jsonl`. Everything else
/// under the edges tree is not owner evidence, each for its own reason:
/// `derived/` and `materialized/` are rebuildable caches the daemon
/// regenerates at will (a working host carries over a hundred gigabytes
/// there; treating them as owner rows blew the streaming budget and made
/// every re-materialization read as the owner moving);
/// `quarantine/<project>/<ts>.jsonl` holds `QuarantineLine` records, not
/// edges, so decoding one as an edge lane marked the whole owner corrupt;
/// and `migrations/<id>/staging/*.jsonl` are retained point-in-time
/// migration artifacts, not live rows the backfill may count or rewrite.
/// A deny-list here silently promoted every future artifact family to
/// owner evidence; new LIVE lane layouts must opt in by name.
fn durable_lane(relative: &Path) -> bool {
    use std::path::Component;

    if relative
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("jsonl")
    {
        return false;
    }
    let mut components = relative.components();
    match (components.next(), components.next(), components.next()) {
        // Top-level legacy combined lane: <project>.jsonl
        (Some(Component::Normal(_)), None, None) => true,
        // Active split lanes: explicit/<project>.jsonl, observed/<project>.jsonl
        (Some(Component::Normal(first)), Some(Component::Normal(_)), None) => {
            first == "explicit" || first == "observed"
        }
        _ => false,
    }
}

pub fn capture_project_catalog_owner_snapshot(
    edges_dir: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotStateV1, build_owner_snapshot, capture_stable_streamed_tree_nofollow,
        corrupt_owner_snapshot, finalize_owner_snapshot, missing_owner_snapshot, owner_subsource,
        sha256_hex,
    };

    match std::fs::symlink_metadata(edges_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("transcript_edge", "transcript_edge:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "transcript_edge",
                "transcript_edge:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures = match capture_stable_streamed_tree_nofollow(
        edges_dir,
        "transcript_edge",
        limits,
        durable_lane,
        |subsource_id: &str| TranscriptEdgeLaneDecoder::new(subsource_id, limits.max_rows),
        // The SAME row walk the stamper uses, so the two halves cannot disagree
        // about which rows exist or what they are called.
        TranscriptEdgeLaneDecoder::decode,
        TranscriptEdgeLaneDecoder::finish,
    ) {
        Ok(captures) => captures,
        Err(error) => {
            return corrupt_owner_snapshot(
                "transcript_edge",
                error
                    .subsource_id
                    .as_deref()
                    .unwrap_or("transcript_edge:root"),
                error.code,
                limits,
            );
        }
    };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "transcript_edge",
            vec![owner_subsource("transcript_edge:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for capture in captures {
        // A lane that is anything other than fully present - vanished mid-walk,
        // unopenable, non-regular - fails the whole capture rather than
        // contributing an empty subsource, because a partial lane set would
        // under-report the legacy selectors still needing migration.
        if !matches!(capture.state, OwnerSnapshotStateV1::Present { .. }) {
            return corrupt_owner_snapshot(
                "transcript_edge",
                &capture.subsource_id,
                "owner_source_unreadable",
                limits,
            );
        }
        subsources.push(owner_subsource(
            capture.subsource_id,
            capture.state,
            &capture.rows,
        ));
        rows.extend(capture.rows);
    }
    finalize_owner_snapshot(
        "transcript_edge",
        "transcript_edge:root",
        subsources,
        rows,
        limits,
    )
}

/// The durable field the Phase 6 backfill stamps onto an edge row.
///
/// Declared here, beside [`Edge::project_id`] and the identity function that
/// must exclude it, so the three cannot drift apart.
pub const EDGE_PROJECT_ID_FIELD: &str = "project_id";

/// The ONE composition of a transcript-edge MEMBER row id, so every half of the
/// backfill names a given lane row identically.
///
/// The ordinal is the row's position among the lane's lines, not a counter over
/// same-content rows. Both discriminate duplicates, but only the ordinal is
/// O(1) to carry: a same-content counter needs a live map of every identity in
/// the lane, which on a real host is millions of entries and defeats the whole
/// point of streaming the file. The ordinal is equally stable across a stamp,
/// because stamping rewrites a line in place and never adds, removes, or
/// reorders one.
fn transcript_edge_stable_row_id(subsource_id: &str, identity: &str, ordinal: u64) -> String {
    format!("{subsource_id}:{identity}:{ordinal}")
}

/// The prefix an AGGREGATE (selector-keyed) row id carries in one lane.
///
/// Separated from the id itself so the apply half can recognise an obligation
/// belonging to a lane without knowing the selector literal, which the plan
/// deliberately never gives it: literal selectors are host-local and stay in the
/// runtime binding set.
fn transcript_edge_selector_row_prefix(subsource_id: &str) -> String {
    format!("{subsource_id}:selector:")
}

/// One aggregate observation id: this lane, this selector.
///
/// Unambiguous against a member id because the literal `selector` component can
/// never be a member's identity, which is always 64 hex characters.
fn transcript_edge_selector_row_id(subsource_id: &str, literal_selector: &str) -> String {
    format!(
        "{}{}",
        transcript_edge_selector_row_prefix(subsource_id),
        transcript_edge_selector_digest(literal_selector)
    )
}

/// The owner's own digest of a selector literal, used to key aggregates and to
/// match rows during apply without ever holding the literal in the plan.
fn transcript_edge_selector_digest(literal_selector: &str) -> String {
    bbox_corpus_core::project_catalog_snapshot::sha256_hex(literal_selector.as_bytes())
}

/// The selector digest `source_row_id` names within this lane, if any.
fn transcript_edge_selector_digest_of<'a>(
    subsource_id: &str,
    source_row_id: &'a str,
) -> Option<&'a str> {
    source_row_id
        .strip_prefix(&transcript_edge_selector_row_prefix(subsource_id))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// One catalog-visible row of one edge lane, as the walk saw it.
struct TranscriptEdgeLaneRowV1 {
    /// Position among the lane's lines, counting every line the file has.
    ordinal: u64,
    cwd: String,
    /// The parsed line, so a caller that stamps or reads the row does not parse
    /// it a second time. Dropped with the row: never accumulated.
    value: serde_json::Value,
}

impl TranscriptEdgeLaneRowV1 {
    /// This row's name inside its selector group's member commitment.
    ///
    /// Every half of the backfill derives it here: capture folds it into the
    /// evidence, and the stamp and the verify refold it from a fresh walk to
    /// prove the group still holds the same rows in the same order.
    fn member_row_id(&self, subsource_id: &str) -> Result<String, &'static str> {
        let identity =
            transcript_edge_value_identity(&self.value).ok_or("transcript_edge_invalid")?;
        Ok(transcript_edge_stable_row_id(
            subsource_id,
            &identity,
            self.ordinal,
        ))
    }
}

/// The per-lane row walk: which lines are catalog-visible rows, what selector
/// each carries, and where it sits.
///
/// Every half of the backfill goes through this one definition - capture,
/// stamp, and verify - so they cannot drift on the filter (blank lines skipped,
/// rows without a nonempty `cwd` skipped, anything else that is not an edge is a
/// corrupt lane) or on row position. State is O(1): one counter.
struct TranscriptEdgeLaneWalk {
    next_ordinal: u64,
}

impl TranscriptEdgeLaneWalk {
    fn new() -> Self {
        Self { next_ordinal: 0 }
    }

    /// Feed the NEXT line of the lane, in order and exactly once.
    ///
    /// `Ok(None)` is "this line contributes no catalog row"; `Err` is a corrupt
    /// lane.
    fn accept(&mut self, content: &str) -> Result<Option<TranscriptEdgeLaneRowV1>, &'static str> {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        if content.trim().is_empty() {
            return Ok(None);
        }
        let value: serde_json::Value =
            serde_json::from_str(content).map_err(|_| "transcript_edge_invalid")?;
        // Validated as a typed Edge to keep capture's existing validity
        // contract - a lane line that is not an edge is a corrupt owner, not a
        // row to skip - but deserialized from the value already parsed, because
        // a second parse of every line of a multi-gigabyte lane is pure cost.
        let edge = Edge::deserialize(&value).map_err(|_| "transcript_edge_invalid")?;
        let Some(cwd) = edge
            .metadata
            .get("cwd")
            .map(|cwd| cwd.trim())
            .filter(|cwd| !cwd.is_empty())
        else {
            return Ok(None);
        };
        Ok(Some(TranscriptEdgeLaneRowV1 {
            ordinal,
            cwd: cwd.to_string(),
            value,
        }))
    }
}

/// The streaming capture's per-lane decoder: it folds every catalog-visible row
/// into its selector's member set and emits ONE row per selector.
///
/// The aggregation is not an optimization, it is the only shape that fits: a
/// working host carries millions of `cwd`-bearing edge rows over a couple of
/// hundred distinct selectors, and the canonical inventory holds a hundred
/// thousand entries in total. Deepest-root classification, planning, and
/// stamping all key on the selector, so per-row rows were evidence nothing
/// consumed at a scale nothing could hold.
struct TranscriptEdgeLaneDecoder {
    subsource_id: String,
    max_selectors: usize,
    walk: TranscriptEdgeLaneWalk,
    selectors: BTreeMap<String, LegacySelectorMembersBuilderV1>,
}

impl TranscriptEdgeLaneDecoder {
    fn new(subsource_id: &str, max_selectors: usize) -> Self {
        Self {
            subsource_id: subsource_id.to_string(),
            max_selectors,
            walk: TranscriptEdgeLaneWalk::new(),
            selectors: BTreeMap::new(),
        }
    }

    fn decode(&mut self, line: &[u8]) -> Result<(), &'static str> {
        // Validated per line rather than per file. Equivalent, because `\n`
        // cannot occur inside a multi-byte UTF-8 sequence, so a lane is valid
        // UTF-8 exactly when all of its lines are.
        let content = std::str::from_utf8(line).map_err(|_| "transcript_edge_invalid")?;
        let Some(row) = self.walk.accept(content)? else {
            return Ok(());
        };
        let member_row_id = row.member_row_id(&self.subsource_id)?;
        let TranscriptEdgeLaneRowV1 { cwd, .. } = row;
        if !self.selectors.contains_key(&cwd) && self.selectors.len() >= self.max_selectors {
            // The distinct-selector map is the decoder's only unbounded
            // structure, so it carries the row ceiling directly rather than
            // waiting for the walker to notice after the lane is finished.
            return Err("owner_row_limit");
        }
        self.selectors.entry(cwd).or_default().push(&member_row_id);
        Ok(())
    }

    fn finish(self) -> Result<Vec<OwnerSnapshotRowV1>, &'static str> {
        let Self {
            subsource_id,
            selectors,
            ..
        } = self;
        Ok(selectors
            .into_iter()
            .map(|(literal_selector, members)| {
                OwnerSnapshotRowV1::legacy_selector_aggregate(
                    transcript_edge_selector_row_id(&subsource_id, &literal_selector),
                    LegacyProjectSelectorKindV1::AbsolutePath,
                    literal_selector,
                    members.finish(),
                )
            })
            .collect())
    }
}

/// The ONE transcript-edge row identity, shared by capture and by stamping.
///
/// BINDING (plan section 3.3, adjudication Q-E1): derived from the complete
/// JSON value with `project_id` REMOVED. Hashing the raw line - which is what
/// capture used to do - would make a row's identity change the instant the
/// backfill stamped it, so a crash-retry could never recognise its own
/// already-stamped work and would re-stamp or refuse it as absent. Excluding
/// the field makes the identity invariant across the write that adds it.
///
/// The COMPLETE value is hashed, not the typed [`Edge`] projection, so a field
/// a newer binary wrote still participates in identity instead of being
/// silently dropped by a round-trip through this binary's struct.
///
/// Object keys are recursively sorted before hashing. `serde_json::Map`
/// iterates in insertion order or sorted order depending on whether the
/// `preserve_order` feature happens to be unified in from elsewhere in the
/// dependency graph; canonicalising makes every stable row id in the corpus
/// independent of that, rather than silently dependent on an unrelated crate's
/// feature selection.
pub fn transcript_edge_row_identity(line: &str) -> Option<String> {
    transcript_edge_value_identity(&serde_json::from_str(line).ok()?)
}

/// [`transcript_edge_row_identity`] for a line the caller has already parsed.
///
/// Borrows and skips `project_id` while canonicalising rather than removing it
/// from a copy. A lane walk hashes every catalog-visible row, and a stamp needs
/// the same value afterwards to rewrite it, so neither may pay for a clone.
fn transcript_edge_value_identity(value: &serde_json::Value) -> Option<String> {
    let canonical = value
        .as_object()?
        .iter()
        .filter(|(key, _)| key.as_str() != EDGE_PROJECT_ID_FIELD)
        .map(|(key, value)| (key.clone(), canonicalize_json_value(value)))
        .collect::<BTreeMap<_, _>>();
    let bytes = serde_json::to_vec(&canonical).ok()?;
    Some(bbox_corpus_core::project_catalog_snapshot::sha256_hex(
        &bytes,
    ))
}

/// Recursively sort object keys so a hash is key-order-independent.
fn canonicalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json_value(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json_value).collect())
        }
        other => other.clone(),
    }
}

/// The lane set: every lane file, paired with the subsource id its rows carry.
///
/// Enumeration only. The apply half opens at most the ONE lane an obligation
/// names, which is what makes stamping a single selector cost one lane rather
/// than the whole tree.
fn transcript_edge_lane_set(
    edges_dir: &Path,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<Vec<(PathBuf, String)>, &'static str> {
    use bbox_corpus_core::project_catalog_snapshot::{
        enumerate_regular_tree_nofollow, stable_subsource_id,
    };

    // The same root authority the capture takes, so an unsafe or absent lane
    // tree refuses here instead of reading as an empty lane set, which would
    // report every obligation as an absent row.
    if NofollowDirectory::open_existing(edges_dir)
        .map_err(|_| "owner_tree_unsafe")?
        .is_none()
    {
        return Err(OWNER_SOURCE_MISSING);
    }
    Ok(
        enumerate_regular_tree_nofollow(edges_dir, limits, durable_lane)
            .map_err(|error| error.code)?
            .into_iter()
            .map(|relative| {
                let subsource_id = stable_subsource_id("transcript_edge", &relative);
                (relative, subsource_id)
            })
            .collect(),
    )
}

/// Stream one lane's lines through `on_line`, returning the lane's content
/// digest, or `None` when the lane is not there.
///
/// The lane is opened ONCE and read through that descriptor, so the lines a
/// caller sees are one coherent version of the file even if it is atomically
/// replaced mid-read. Memory is one chunk plus one line, whatever the lane
/// weighs.
// The backfill apply path is an offline admin path, not a tool handler.
#[allow(clippy::disallowed_methods)]
fn stream_lane_nofollow(
    directory: &NofollowDirectory,
    name: &str,
    limits: OwnerSnapshotLimitsV1,
    mut on_line: impl FnMut(&[u8], &[u8]) -> std::result::Result<(), &'static str>,
) -> std::result::Result<Option<String>, &'static str> {
    use std::io::Read as _;

    let Some(mut file) = directory
        .open_regular(name, "edge lane")
        .map_err(|_| OWNER_SOURCE_UNREADABLE)?
    else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    let mut budget = limits.max_streamed_source_bytes;
    let mut chunk = vec![0u8; STREAMED_CHUNK_BYTES];
    let mut splitter = StreamedLineSplitterV1::new(limits.max_streamed_line_bytes);
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(OWNER_SOURCE_UNREADABLE),
        };
        budget = budget
            .checked_sub(read as u64)
            .ok_or("owner_source_byte_limit")?;
        hasher.update(&chunk[..read]);
        splitter.push_chunk(&chunk[..read], &mut on_line)?;
    }
    splitter.finish(on_line)?;
    Ok(Some(hex::encode(hasher.finalize())))
}

/// Whether the lane still holds exactly the bytes a rewrite was computed from.
///
/// Split out and digest-shaped for two reasons: the stamper cannot hold a
/// gigabyte lane in memory to compare bytes, and the recheck is the one step a
/// test must be able to drive directly, because racing a real concurrent writer
/// would make it timing-dependent.
fn transcript_edge_lane_unchanged(
    directory: &NofollowDirectory,
    name: &str,
    expected_digest: &str,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<(), &'static str> {
    match stream_lane_nofollow(directory, name, limits, |_content, _terminator| Ok(())) {
        Ok(Some(current)) if current == expected_digest => Ok(()),
        // The lane moved under us. Abandon rather than clobber the concurrent
        // writer; nothing has been committed at this point, so the caller can
        // retry against the new state.
        Ok(_) => Err(OWNER_SOURCE_MOVED),
        Err(error) => Err(error),
    }
}

/// Stamp every transcript-edge row of ONE lane carrying one legacy selector.
///
/// `source_row_id` names a (lane, selector) pair, not a single row. That is the
/// shape the plan actually has: a working host carries millions of `cwd`-bearing
/// edge rows over a couple of hundred distinct selectors, deepest-root
/// classification maps the SELECTOR, and a per-row ledger of the members could
/// not fit the canonical inventory at all. The obligation is therefore "give
/// every row in this lane with this selector this project id".
///
/// Because a group never spans lanes, applying it is exactly ONE atomic
/// replacement, so an obligation is never half applied: a crash leaves either
/// the old lane or the fully stamped one, and the temporary it may leave behind
/// is not a `.jsonl` file and so is invisible to capture.
///
/// The physical write is the whole-file replacement plan section 3.3 (Q-E1)
/// specifies, done by STREAMING: lines are read through one descriptor, matching
/// rows are transformed through `serde_json::Value`, every other line is copied
/// through byte for byte including its exact terminator, and the result lands in
/// a unique sibling temporary that is fsynced, atomically renamed over the lane,
/// and followed by a parent fsync. Never an in-place overwrite, which would
/// expose a torn line to a concurrent reader, and never an appended superseding
/// duplicate, which would give the lane two rows at one position.
///
/// Edge lane writes normally run on the reindex writer actor, which the backfill
/// is not. The plan's sanctioned alternative is taken instead: a
/// descriptor-confined source-identity recheck immediately before the
/// replacement, so a lane that changed between the read and the rename refuses
/// rather than clobbering the concurrent writer's work.
pub fn stamp_project_catalog_owner_row(
    edges_dir: &Path,
    source_row_id: &str,
    expected_members: &LegacySelectorMembersV1,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
    if project_id.trim().is_empty() {
        return Err(OwnerRowStampError::new(OWNER_PROJECT_ID_INVALID));
    }
    let lanes = transcript_edge_lane_set(edges_dir, limits).map_err(OwnerRowStampError::new)?;
    for (relative, subsource_id) in lanes {
        let Some(selector_digest) =
            transcript_edge_selector_digest_of(&subsource_id, source_row_id)
        else {
            continue;
        };
        return stamp_lane_selector(
            &edges_dir.join(&relative),
            &subsource_id,
            selector_digest,
            expected_members,
            project_id,
            limits,
        )
        .map_err(OwnerRowStampError::new);
    }
    Err(OwnerRowStampError::new(OWNER_ROW_ABSENT))
}

/// Rewrite one lane, stamping its selector group, and clean up after a failure.
fn stamp_lane_selector(
    path: &Path,
    subsource_id: &str,
    selector_digest: &str,
    expected_members: &LegacySelectorMembersV1,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<OwnerRowStampOutcomeV1, &'static str> {
    let parent = path.parent().ok_or(OWNER_SOURCE_UNWRITABLE)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(OWNER_SOURCE_UNWRITABLE)?;
    let directory = NofollowDirectory::open_existing(parent)
        .map_err(|_| OWNER_SOURCE_UNREADABLE)?
        .ok_or(OWNER_SOURCE_MISSING)?;
    let tmp_path = parent.join(format!(
        "{name}.stamp.tmp.{pid}.{seq}",
        pid = std::process::id(),
        seq = writer_temp_sequence()
    ));

    let outcome = rewrite_lane_stamping_selector(
        &directory,
        path,
        name,
        &tmp_path,
        subsource_id,
        selector_digest,
        expected_members,
        project_id,
        limits,
    );
    if !matches!(outcome, Ok(OwnerRowStampOutcomeV1::Stamped)) {
        // Nothing was committed, so the temporary is the only trace of the
        // attempt and must not be left behind. A committed rewrite renamed it
        // away already, which is why this is not an unconditional unlink.
        let _ = fs::remove_file(&tmp_path);
    }
    outcome
}

/// The streaming rewrite itself, and the only place a lane is written by the
/// backfill.
// The backfill stamper is an offline admin path, not a tool handler.
#[allow(clippy::disallowed_methods)]
fn rewrite_lane_stamping_selector(
    directory: &NofollowDirectory,
    path: &Path,
    name: &str,
    tmp_path: &Path,
    subsource_id: &str,
    selector_digest: &str,
    expected_members: &LegacySelectorMembersV1,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<OwnerRowStampOutcomeV1, &'static str> {
    let temporary = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(tmp_path)
        .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    let mut writer = BufWriter::new(temporary);
    let mut walk = TranscriptEdgeLaneWalk::new();
    let mut matched = 0u64;
    let mut stamped = 0u64;
    // Refolded from THIS walk, in the same pass that writes the temporary, so
    // the evidence compared below describes the bytes about to be replaced
    // rather than some earlier read of the lane.
    let mut members = LegacySelectorMembersBuilderV1::new();

    let digest = stream_lane_nofollow(directory, name, limits, |content, terminator| {
        let text = std::str::from_utf8(content).map_err(|_| "transcript_edge_invalid")?;
        let mut rewritten = None;
        if let Some(row) = walk.accept(text)?
            && transcript_edge_selector_digest(&row.cwd) == selector_digest
        {
            matched += 1;
            members.push(&row.member_row_id(subsource_id)?);
            let mut value = row.value;
            // The shared three-way rule: unstamped writes, same-project elides,
            // different-project refuses. Never re-implemented here.
            match stamp_row_object(&mut value, project_id).map_err(|error| error.code)? {
                RowStampDecisionV1::AlreadyStamped => {}
                RowStampDecisionV1::Write => {
                    stamped += 1;
                    rewritten =
                        Some(serde_json::to_string(&value).map_err(|_| OWNER_SOURCE_UNWRITABLE)?);
                }
            }
        }
        match &rewritten {
            Some(line) => writer.write_all(line.as_bytes()),
            None => writer.write_all(content),
        }
        .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
        writer
            .write_all(terminator)
            .map_err(|_| OWNER_SOURCE_UNWRITABLE)
    })?
    .ok_or(OWNER_SOURCE_MISSING)?;

    if matched == 0 {
        // The lane exists but holds no row with this selector: the group form of
        // naming a row the owner does not have. Kept ahead of the member check
        // because a vanished group deserves the sharper diagnostic.
        return Err(OWNER_ROW_ABSENT);
    }
    // BEFORE the write, and before the already-stamped shortcut. Capture records
    // a count and an ordered commitment precisely so a removed, duplicated, or
    // substituted member is detectable, and that evidence is worth nothing
    // unless something rederives it at the moment of writing: a group whose
    // members changed while staying uniformly stamped would otherwise be
    // stamped, and later verified, as if it were the reviewed set.
    //
    // Stamping cannot itself move this: identity excludes `project_id` and the
    // rewrite replaces a line in place, so a completed obligation still refolds
    // to the commitment it was planned against, which is what keeps a crash
    // retry idempotent rather than refusing its own work.
    ensure_selector_members_unchanged(expected_members, &members.finish())
        .map_err(|error| error.code)?;
    if stamped == 0 {
        // Every member already carries exactly this project id, so the re-apply
        // of a completed obligation writes nothing at all. That is what makes a
        // torn backfill safe to repeat.
        return Ok(OwnerRowStampOutcomeV1::AlreadyStamped);
    }

    let mut temporary = writer.into_inner().map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    temporary.flush().map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    temporary.sync_all().map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    // The recheck, as late as it can be: everything above is reversible by
    // unlinking the temporary, and nothing below can observe a change.
    transcript_edge_lane_unchanged(directory, name, &digest, limits)?;
    fs::rename(tmp_path, path).map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    // Durability of the rename itself, not of the bytes: without this the
    // directory entry can be lost even though the temporary was fsynced.
    fs::File::open(path.parent().ok_or(OWNER_SOURCE_UNWRITABLE)?)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    Ok(OwnerRowStampOutcomeV1::Stamped)
}

/// What one lane's rows say about the project id of ONE selector group.
///
/// A group is stamped only when EVERY member carries the same id. Anything else
/// is not a stamped group, and verify must refuse it rather than accept a
/// majority.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectorProjectFoldV1 {
    Unseen,
    Unstamped,
    Stamped(String),
    /// Members disagree: some stamped and some not, or stamped with different
    /// project ids.
    Divergent,
}

impl SelectorProjectFoldV1 {
    fn observe(&mut self, observed: OwnerRowProjectIdV1) {
        *self = match (std::mem::replace(self, Self::Divergent), observed) {
            (Self::Divergent, _) => Self::Divergent,
            (Self::Unseen, OwnerRowProjectIdV1::Unstamped) => Self::Unstamped,
            (Self::Unseen, OwnerRowProjectIdV1::Stamped(project)) => Self::Stamped(project),
            (Self::Unstamped, OwnerRowProjectIdV1::Unstamped) => Self::Unstamped,
            (Self::Stamped(held), OwnerRowProjectIdV1::Stamped(project)) if held == project => {
                Self::Stamped(held)
            }
            _ => Self::Divergent,
        };
    }

    /// `None` is "this owner holds no such group", which verify reads as an
    /// absent row.
    fn into_observation(self) -> Option<OwnerRowProjectIdV1> {
        match self {
            Self::Unseen => None,
            Self::Unstamped => Some(OwnerRowProjectIdV1::Unstamped),
            Self::Stamped(project) => Some(OwnerRowProjectIdV1::Stamped(project)),
            // A divergent group reports as UNSTAMPED rather than as one of the
            // ids it partly carries. Verify refuses either way, and the owner
            // must not invent a uniform answer for a group that does not have
            // one.
            Self::Divergent => Some(OwnerRowProjectIdV1::Unstamped),
        }
    }
}

/// Read the stable project ids of MANY transcript-edge selector groups, the
/// VERIFY half of [`stamp_project_catalog_owner_row`].
///
/// ONE walk of the lane set answers every requested id, and only lanes that own
/// a requested id are opened at all. This owner is the worst case for a per-row
/// reader: each row would re-walk the lane set, and the answers would come from
/// as many different durable states as there were requested rows.
pub fn read_project_catalog_owner_rows(
    edges_dir: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerRowBatchV1, note_owner_row_read_capture,
    };

    note_owner_row_read_capture();
    let lanes = transcript_edge_lane_set(edges_dir, limits).map_err(OwnerRowStampError::new)?;
    let mut batch = OwnerRowBatchV1::new();
    for (relative, subsource_id) in lanes {
        // Which requested obligations belong to THIS lane, keyed by the digest
        // its rows will be matched on. Each carries the member evidence it was
        // planned against, refolded below from the same walk that answers it.
        let mut wanted = rows
            .iter()
            .filter_map(|(source_row_id, expected)| {
                transcript_edge_selector_digest_of(&subsource_id, source_row_id).map(|digest| {
                    (
                        digest.to_string(),
                        SelectorGroupReadV1 {
                            source_row_id: source_row_id.clone(),
                            expected: expected.clone(),
                            members: LegacySelectorMembersBuilderV1::new(),
                            fold: SelectorProjectFoldV1::Unseen,
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        if wanted.is_empty() {
            // Never opened: a lane owning none of the requested ids has nothing
            // to say, and reading it would be pure I/O.
            continue;
        }
        let path = edges_dir.join(&relative);
        let parent = path
            .parent()
            .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_UNREADABLE))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| OwnerRowStampError::new(OWNER_SOURCE_UNREADABLE))?;
        let Some(directory) = NofollowDirectory::open_existing(parent)
            .map_err(|_| OwnerRowStampError::new(OWNER_SOURCE_UNREADABLE))?
        else {
            continue;
        };
        let mut walk = TranscriptEdgeLaneWalk::new();
        stream_lane_nofollow(&directory, name, limits, |content, _terminator| {
            let text = std::str::from_utf8(content).map_err(|_| "transcript_edge_invalid")?;
            let Some(row) = walk.accept(text)? else {
                return Ok(());
            };
            let Some(group) = wanted.get_mut(&transcript_edge_selector_digest(&row.cwd)) else {
                return Ok(());
            };
            group.members.push(&row.member_row_id(&subsource_id)?);
            group
                .fold
                .observe(read_row_object_project_id(&row.value).map_err(|error| error.code)?);
            Ok(())
        })
        .map_err(OwnerRowStampError::new)?;
        for group in wanted.into_values() {
            let SelectorGroupReadV1 {
                source_row_id,
                expected,
                members,
                fold,
            } = group;
            let Some(observation) = fold.into_observation() else {
                // The owner holds no such group at all. Left absent from the
                // batch, which verify already refuses; refolding evidence for a
                // group that is not there would say nothing more.
                continue;
            };
            // A group that IS there must still be the group the plan reviewed.
            // Without this, a member removed or substituted after the stamp
            // leaves the survivors uniformly stamped and verify reports a clean
            // apply over a set nobody approved.
            ensure_selector_members_unchanged(&expected, &members.finish())?;
            batch.insert(source_row_id, observation);
        }
    }
    Ok(batch)
}

/// One requested selector group, as the verify walk builds its answer.
struct SelectorGroupReadV1 {
    source_row_id: String,
    expected: LegacySelectorMembersV1,
    members: LegacySelectorMembersBuilderV1,
    fold: SelectorProjectFoldV1,
}

impl Edge {
    pub fn dedup_key(&self) -> EdgeKey {
        EdgeKey {
            source: self.source.clone(),
            kind: self.kind.clone(),
            target: self.target.clone(),
            provenance: self.provenance,
            confidence: self.confidence,
        }
    }
}

pub fn count_materialized_jsonl_files(edges_dir: &Path) -> usize {
    let mat_dir = crate::manifest::materialized_dir(edges_dir);
    if !mat_dir.is_dir() {
        return 0;
    }
    fn count_jsonl_recursive(dir: &Path) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    count += count_jsonl_recursive(&path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    count += 1;
                }
            }
        }
        count
    }
    count_jsonl_recursive(&mat_dir)
}

pub fn scan_lane_project_ids(lane_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(entries) = fs::read_dir(lane_dir) else {
        return ids;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = sidecar_file_stem(&path) {
            ids.insert(stem.to_string());
        }
    }
    ids
}

pub fn sidecar_project_id_is_registered(
    project_id: &str,
    registered: Option<&HashSet<String>>,
) -> bool {
    let Some(registered) = registered else {
        return true;
    };
    registered.contains(project_id)
}

pub fn edges_dir_from_bro_store(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| store_dir.join("edges"))
}

pub fn edges_dir_from_projects_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| PathBuf::from("edges"))
}

pub fn managed_derived_edges_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("derived")
}

pub fn sidecar_file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}

pub fn scan_managed_derived_project_ids(managed_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(namespace_entries) = fs::read_dir(managed_dir) else {
        return ids;
    };
    for ns_entry in namespace_entries.filter_map(Result::ok) {
        let ns_path = ns_entry.path();
        if !ns_path.is_dir() {
            continue;
        }
        let Ok(project_entries) = fs::read_dir(&ns_path) else {
            continue;
        };
        for proj_entry in project_entries.filter_map(Result::ok) {
            let proj_path = proj_entry.path();
            if proj_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = sidecar_file_stem(&proj_path) {
                ids.insert(stem.to_string());
            }
        }
    }
    ids
}

pub fn sidecar_project_is_registered(
    path: &Path,
    registered_project_ids: Option<&HashSet<String>>,
) -> bool {
    let Some(registered_project_ids) = registered_project_ids else {
        return true;
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if matches!(stem, "agents") {
        return true;
    }
    registered_project_ids.contains(stem)
}

/// Test-fixture helper: append raw chunker edges to a project's JSONL lane.
/// Deliberately un-gated (no `#[cfg(test)]`) so consumer-crate tests can use
/// it — `cfg(test)` does not cross crate boundaries.
pub fn append_project_edges(
    edges_dir: &Path,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
            // Chunker-derived edges carry no catalog authority; only the
            // Phase 6 backfill stamps a project onto an existing row.
            project_id: None,
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

// Unique per-process sequence counter for writer temp files. Using
// create_new (O_EXCL) with pid+seq guarantees a fresh inode every time,
// so GC cannot unlink a temp the writer is actively using via a
// deterministic name (R16F2).
static WRITER_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn writer_temp_sequence() -> u64 {
    WRITER_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// edge sidecar writes run on the reindex/writer-actor thread.
#[allow(clippy::disallowed_methods)]
pub fn replace_project_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    let dir = managed_derived_edges_dir(edges_dir).join(namespace);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{project_id}.jsonl"));
    if edges.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }

    let tmp_path = dir.join(format!(
        "{project_id}.jsonl.tmp.{pid}.{seq}",
        pid = std::process::id(),
        seq = writer_temp_sequence()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)?;
    // Buffered: one syscall per ~8KiB instead of one per serialized fragment
    // (the unbuffered loop dominated reindex project phases; thread-935b467d).
    let mut writer = BufWriter::new(file);
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
            // Chunker-derived edges carry no catalog authority; only the
            // Phase 6 backfill stamps a project onto an existing row.
            project_id: None,
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    drop(file);
    if let Err(err) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}

pub fn append_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    for edge in edges {
        serde_json::to_writer(&mut writer, edge)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

pub fn append_edges_dedup(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<usize> {
    if edges.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut seen = HashSet::new();
    if let Ok(file) = fs::File::open(&path) {
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(edge) = serde_json::from_str::<Edge>(&line) {
                seen.insert(edge_import_key(&edge));
            }
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    let mut written = 0usize;
    for edge in edges {
        if !seen.insert(edge_import_key(edge)) {
            continue;
        }
        serde_json::to_writer(&mut writer, edge)?;
        writer.write_all(b"\n")?;
        written += 1;
    }
    writer.flush()?;
    Ok(written)
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeSidecarCompactionStats {
    pub project_id: String,
    pub applied: bool,
    pub existed: bool,
    pub legacy_path: String,
    pub backup_path: Option<String>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub lines_seen: u64,
    pub retained_lines: u64,
    pub derived_edges_removed: u64,
    pub explicit_edges_retained: u64,
    pub malformed_lines_retained: u64,
    pub blank_lines_dropped: u64,
}

// invoked from bbox_edge_compact's run_blocking closure.
#[allow(clippy::disallowed_methods)]
pub fn compact_legacy_sidecar(
    edges_dir: &Path,
    project_id: &str,
    apply: bool,
) -> Result<EdgeSidecarCompactionStats> {
    let project_id = bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_owned())
        .context("validating edge sidecar project id")?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut stats = EdgeSidecarCompactionStats {
        project_id: project_id.to_string(),
        applied: false,
        existed: path.exists(),
        legacy_path: path.display().to_string(),
        backup_path: None,
        bytes_before: 0,
        bytes_after: 0,
        lines_seen: 0,
        retained_lines: 0,
        derived_edges_removed: 0,
        explicit_edges_retained: 0,
        malformed_lines_retained: 0,
        blank_lines_dropped: 0,
    };
    if !path.exists() {
        return Ok(stats);
    }

    stats.bytes_before = fs::metadata(&path)?.len();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let nonce = COMPACTION_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_file_name(format!(
        "{project_id}.jsonl.compact-{stamp}-{}-{nonce}.tmp",
        std::process::id(),
    ));
    let mut writer = if apply {
        Some(BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp_path)?,
        ))
    } else {
        None
    };

    let file = fs::File::open(&path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        stats.lines_seen += 1;
        if line.trim().is_empty() {
            stats.blank_lines_dropped += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(&line) {
            Ok(edge) if edge.provenance == EdgeProvenance::Derived => {
                stats.derived_edges_removed += 1;
            }
            Ok(_) => {
                stats.explicit_edges_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            Err(_) => {
                stats.malformed_lines_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
        }
    }

    if !apply || stats.derived_edges_removed == 0 && stats.blank_lines_dropped == 0 {
        if let Some(mut writer) = writer {
            writer.flush()?;
            drop(writer);
            let _ = fs::remove_file(&tmp_path);
        }
        return Ok(stats);
    }

    let backup_path = path.with_file_name(format!(
        "{project_id}.jsonl.bak-{stamp}-{}-{nonce}",
        std::process::id()
    ));
    if let Some(mut writer) = writer {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    } else {
        anyhow::bail!("internal error: compaction apply requested without writer");
    }
    // Preserve the old inode without ever removing the live lane. A crash
    // before the replacement leaves the original path intact; a crash after
    // it leaves the new path plus this hard-linked backup. The old
    // rename-away/rename-in sequence exposed an absence window in which every
    // reader treated the project as having no legacy edges.
    fs::hard_link(&path, &backup_path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    match fs::rename(&tmp_path, &path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            let _ = fs::remove_file(&backup_path);
            return Err(err.into());
        }
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    stats.applied = true;
    stats.backup_path = Some(backup_path.display().to_string());
    Ok(stats)
}

pub fn edge_import_key(edge: &Edge) -> String {
    let mut hasher = Sha256::new();
    hasher.update(edge.source.to_string());
    hasher.update(b"\0");
    hasher.update(&edge.kind);
    hasher.update(b"\0");
    hasher.update(edge.target.to_string());
    hasher.update(b"\0");
    if let Some(commit) = edge.metadata.get("anchor.commit_sha_at_edit") {
        hasher.update(commit);
    }
    hex::encode(hasher.finalize())
}

pub fn derived_tool_projection(edge: &Edge) -> Option<Edge> {
    if edge.kind != "EDITED_FILE" {
        return None;
    }
    let EntityRef::Transcript {
        provider,
        session_id,
        ..
    } = &edge.source
    else {
        return None;
    };
    Some(Edge {
        source: edge.target.clone(),
        kind: "EDITED_BY_SESSION".to_string(),
        target: EntityRef::Session {
            provider: provider.clone(),
            session_id: session_id.clone(),
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
        metadata: edge.metadata.clone(),
        project_id: edge.project_id.clone(),
    })
}

pub fn exact_edge(
    source: EntityRef,
    kind: &str,
    target: EntityRef,
    provenance: EdgeProvenance,
) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance,
        confidence: EdgeConfidence::Exact,
        metadata: BTreeMap::new(),
        project_id: None,
    }
}

pub fn line_provenance_is_derived(line: &str) -> bool {
    let Some(pos) = line.find("\"provenance\"") else {
        return false;
    };
    let rest = &line[pos + "\"provenance\"".len()..];
    let rest = rest.trim_start();
    if !rest.starts_with(':') {
        return false;
    }
    let after_colon = rest[1..].trim_start();
    after_colon.starts_with("\"derived\"")
}

// ---------------------------------------------------------------------------
// Phase 2: Lifecycle-specific write APIs
// ---------------------------------------------------------------------------
//
// Caller audit (recorded here so it does not rot):
//
//   materialized  = computed current workspace/repo view (Derived provenance)
//   observed      = event/provenance history, usually Tool provenance (Explicit)
//   explicit      = user/agent-authored durable fact (Explicit)
//   global        = non-project graph support (Explicit)
//
// append_project_edges callers (legacy append path):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges callers (full Edge with metadata):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges_dedup callers:
//   (none — all production callers moved to lifecycle APIs)
//
// replace_project_edges callers (managed derived replacement):
//   (none directly — wrapped by lifecycle APIs below)
//
// Lifecycle API routing:
//   project_files.rs  → replace_materialized_edges_incremental ("project")
//   git_history.rs    → replace_materialized_edges (full) or merge_materialized_edges (incremental) ("git")
//   tool_edges.rs     → append_observed_edges
//   provenance.rs     → append_explicit_edges
//   routes.rs         → append_explicit_edges (global agents.jsonl)
//   workflow/ops.rs   → append_explicit_edges
// ---------------------------------------------------------------------------

pub fn append_explicit_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<usize> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_explicit_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges_dedup(edges_dir, project_id, edges)
}

pub fn append_observed_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_observed_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges(edges_dir, project_id, edges)
}

pub fn replace_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    replace_project_edges(edges_dir, namespace, project_id, edges)
}

pub fn read_managed_derived_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
) -> Result<Vec<Edge>> {
    let path = managed_derived_edges_dir(edges_dir)
        .join(namespace)
        .join(format!("{project_id}.jsonl"));
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let reader = std::io::BufReader::new(file);
    let mut edges = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
            edges.push(edge);
        }
    }
    Ok(edges)
}

pub fn rel_path_hashes_of(edges: &[bbox_chunker::Edge]) -> HashSet<String> {
    let mut hashes = HashSet::new();
    for e in edges {
        if let EntityRef::ProjectFile { rel_path_hash, .. }
        | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &e.source
        {
            hashes.insert(rel_path_hash.clone());
        }
        if let EntityRef::ProjectFile { rel_path_hash, .. }
        | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &e.target
        {
            hashes.insert(rel_path_hash.clone());
        }
    }
    hashes
}

pub fn edge_touches_any_path_hash(edge: &Edge, stale_hashes: &HashSet<String>) -> bool {
    match (&edge.source, &edge.target) {
        (EntityRef::ProjectFile { rel_path_hash, .. }, _)
        | (EntityRef::ProjectFileV2 { rel_path_hash, .. }, _)
        | (_, EntityRef::ProjectFile { rel_path_hash, .. }) => stale_hashes.contains(rel_path_hash),
        (_, EntityRef::ProjectFileV2 { rel_path_hash, .. }) => stale_hashes.contains(rel_path_hash),
        _ => false,
    }
}

pub fn replace_materialized_edges_incremental(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges_incremental: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let stale_hashes = rel_path_hashes_of(new_edges);
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let preserved: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .filter(|e| !edge_touches_any_path_hash(e, &stale_hashes))
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    let mut merged = preserved;
    merged.extend_from_slice(new_edges);
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

/// Drop managed derived edges whose source or target is a project file in
/// `stale_hashes` (rel_path_hash). Used to purge a deleted file's file-anchored
/// edges, which the mtime/size incremental path never revisits once the file is
/// gone from disk. Returns the number of edges removed. Granularity matches
/// `edge_touches_any_path_hash` (the incremental-replace key), so symbol→symbol
/// edges carrying no project-file ref are not removed here.
pub fn purge_managed_edges_for_path_hashes(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    stale_hashes: &HashSet<String>,
) -> Result<usize> {
    if stale_hashes.is_empty() {
        return Ok(0);
    }
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let before = existing.len();
    let retained: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .filter(|e| !edge_touches_any_path_hash(e, stale_hashes))
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    let purged = before.saturating_sub(retained.len());
    if purged > 0 {
        replace_project_edges(edges_dir, namespace, project_id, &retained)?;
    }
    Ok(purged)
}

pub fn merge_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "merge_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let mut seen: HashSet<String> = existing.iter().map(edge_import_key).collect();
    let mut merged: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    for e in new_edges {
        let key = edge_import_key(&Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: BTreeMap::new(),
            project_id: None,
        });
        if seen.insert(key) {
            merged.push(e.clone());
        }
    }
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

// ---------------------------------------------------------------------------
// Phase 2: Legacy edge extraction dry-run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyExtractionPlan {
    pub project_id: String,
    pub legacy_path: String,
    pub total_lines: u64,
    pub derived_lines: u64,
    pub tool_lines: u64,
    pub explicit_lines: u64,
    pub malformed_lines: u64,
    pub blank_lines: u64,
    pub managed_replacement_exists: bool,
    pub extractable: bool,
}

pub fn plan_legacy_edge_extraction(
    edges_dir: &Path,
    project_id: &str,
) -> Result<LegacyExtractionPlan> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut plan = LegacyExtractionPlan {
        project_id: project_id.to_string(),
        legacy_path: legacy_path.display().to_string(),
        ..Default::default()
    };

    let managed = managed_derived_edges_dir(edges_dir);
    plan.managed_replacement_exists = managed
        .join("project")
        .join(format!("{project_id}.jsonl"))
        .exists()
        || managed
            .join("git")
            .join(format!("{project_id}.jsonl"))
            .exists();

    let file = match fs::File::open(&legacy_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(plan),
        Err(e) => return Err(e.into()),
    };

    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        plan.total_lines += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            plan.blank_lines += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(trimmed) {
            Ok(edge) => match edge.provenance {
                EdgeProvenance::Derived => plan.derived_lines += 1,
                EdgeProvenance::Explicit => {
                    let is_tool = edge.kind == "READ_FILE"
                        || edge.kind == "EDITED_FILE"
                        || edge.kind == "RAN_BASH";
                    if is_tool {
                        plan.tool_lines += 1;
                    } else {
                        plan.explicit_lines += 1;
                    }
                }
                EdgeProvenance::Implicit => plan.explicit_lines += 1,
            },
            Err(_) => plan.malformed_lines += 1,
        }
    }

    plan.extractable = plan.managed_replacement_exists && plan.derived_lines > 0;
    Ok(plan)
}

#[cfg(test)]
mod project_catalog_snapshot_tests {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
    };

    #[test]
    fn migration_snapshot_is_no_create_and_captures_only_literal_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let missing =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!root.exists());

        std::fs::create_dir(&root).unwrap();
        let mut with_cwd = Edge {
            source: EntityRef::parse("task:one").unwrap(),
            kind: "RAN_BASH".into(),
            target: EntityRef::parse("task:two").unwrap(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        };
        with_cwd
            .metadata
            .insert("cwd".into(), "/repo/worktree".into());
        let without_cwd = Edge {
            source: EntityRef::parse("task:two").unwrap(),
            kind: "RELATED_TO".into(),
            target: EntityRef::parse("task:one").unwrap(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        };
        std::fs::write(
            root.join("tool.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&with_cwd).unwrap(),
                serde_json::to_string(&without_cwd).unwrap()
            ),
        )
        .unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert!(matches!(
            &snapshot.rows[0].value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector,
                ..
            } if literal_selector == "/repo/worktree"
        ));
    }

    /// One lane holding a stampable row, an unrelated row that must survive
    /// untouched, and a row carrying a field this binary's `Edge` does not know.
    fn lane_fixture(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join("tool.jsonl");
        std::fs::write(
            &path,
            // Hand-written rather than serialized so the unknown field
            // `future_field` and the key order are exactly what a NEWER binary
            // would have written.
            concat!(
                r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"},"future_field":{"written_by":"a newer binary"}}"#,
                "\n",
                r#"{"source":{"type":"task","task_id":"three"},"kind":"RELATED_TO","target":{"type":"task","task_id":"four"},"provenance":"derived","confidence":"exact"}"#,
                "\n",
                r#"{"source":{"type":"task","task_id":"five"},"kind":"RAN_BASH","target":{"type":"task","task_id":"six"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/two"}}"#,
                "\n",
            ),
        )
        .unwrap();
        path
    }

    fn only_row_id(root: &Path, cwd: &str) -> String {
        let snapshot =
            capture_project_catalog_owner_snapshot(root, OwnerSnapshotLimitsV1::default()).unwrap();
        snapshot
            .rows
            .iter()
            .find(|row| {
                matches!(&row.value, OwnerSnapshotRowValueV1::LegacyProjectSelector {
                    literal_selector, ..
                } if literal_selector == cwd)
            })
            .unwrap_or_else(|| panic!("no row for {cwd}"))
            .stable_row_id
            .clone()
    }

    fn selector_members(row: &OwnerSnapshotRowV1) -> LegacySelectorMembersV1 {
        match &row.value {
            OwnerSnapshotRowValueV1::LegacyProjectSelector { members, .. } => members.clone(),
            OwnerSnapshotRowValueV1::InventoryTarget { .. } => {
                panic!("transcript-edge rows are legacy selectors")
            }
        }
    }

    /// The member evidence the CURRENT capture reports for one observation id.
    ///
    /// A plan carries what capture saw; a test that is not exercising the
    /// evidence check takes it from the same place. An id the capture no longer
    /// holds falls back to a singleton, which is the shape a stale plan would
    /// carry and which the owner refuses on its own terms.
    fn captured_members(
        root: &Path,
        row_id: &str,
        limits: OwnerSnapshotLimitsV1,
    ) -> LegacySelectorMembersV1 {
        capture_project_catalog_owner_snapshot(root, limits)
            .unwrap()
            .rows
            .iter()
            .find(|row| row.stable_row_id == row_id)
            .map(selector_members)
            .unwrap_or_else(|| {
                bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row_id)
            })
    }

    /// Stamp a selector group with the evidence its current capture reports.
    fn stamp_group(
        root: &Path,
        row_id: &str,
        project_id: &str,
        limits: OwnerSnapshotLimitsV1,
    ) -> std::result::Result<OwnerRowStampOutcomeV1, OwnerRowStampError> {
        let members = captured_members(root, row_id, limits);
        stamp_project_catalog_owner_row(root, row_id, &members, project_id, limits)
    }

    /// The requested rows of a batched read, each with the evidence its current
    /// capture reports.
    fn read_request(
        root: &Path,
        row_ids: &[&str],
        limits: OwnerSnapshotLimitsV1,
    ) -> bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1 {
        row_ids
            .iter()
            .map(|row_id| {
                (
                    (*row_id).to_string(),
                    captured_members(root, row_id, limits),
                )
            })
            .collect()
    }

    /// THE Q-E1 INVARIANT. A row's identity is the same before and after it is
    /// stamped, so a crash-retry recognises its own completed work.
    ///
    /// Without this the retry would compute a different id, fail to find the
    /// row, and report `owner_row_absent` on a row it had just written.
    #[test]
    fn stamping_leaves_the_row_identity_unchanged_and_retry_sees_already_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");

        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", OwnerSnapshotLimitsV1::default()).unwrap(),
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1::Stamped
        );

        // Re-derived from the POST-stamp file: byte-identical to the id the
        // pre-stamp capture produced.
        assert_eq!(only_row_id(&root, "/repo/one"), row_id);

        // The crash-retry: the exact same call the torn backfill would repeat.
        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", OwnerSnapshotLimitsV1::default()).unwrap(),
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1::AlreadyStamped
        );
    }

    /// The rewrite is a whole-file replacement that preserves every unrelated
    /// line and every field this binary does not know about, and it NEVER
    /// appends a superseding duplicate.
    #[test]
    fn stamping_preserves_unrelated_lines_and_unknown_fields_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let before = std::fs::read_to_string(&path).unwrap();
        let row_id = only_row_id(&root, "/repo/one");

        stamp_group(&root, &row_id, "a1b2c3d4", OwnerSnapshotLimitsV1::default()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        // No append-duplicate: same line count, same order.
        assert_eq!(after_lines.len(), before_lines.len());
        // The two rows this stamp did not name are byte-identical.
        assert_eq!(after_lines[1], before_lines[1]);
        assert_eq!(after_lines[2], before_lines[2]);

        let stamped: serde_json::Value = serde_json::from_str(after_lines[0]).unwrap();
        assert_eq!(stamped["project_id"], "a1b2c3d4");
        // The field this binary's Edge struct has never heard of survived the
        // Value round-trip rather than being dropped.
        assert_eq!(stamped["future_field"]["written_by"], "a newer binary");
        assert_eq!(stamped["metadata"]["cwd"], "/repo/one");
    }

    /// A row already bound to a DIFFERENT project is refused, not overwritten,
    /// and the lane is left exactly as it was.
    #[test]
    fn stamping_a_conflicting_row_refuses_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");
        stamp_group(&root, &row_id, "a1b2c3d4", OwnerSnapshotLimitsV1::default()).unwrap();
        let after_first = std::fs::read(&path).unwrap();

        let error =
            stamp_group(&root, &row_id, "99999999", OwnerSnapshotLimitsV1::default()).unwrap_err();

        assert_eq!(
            error.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_PROJECT_ID_CONFLICT
        );
        assert_eq!(std::fs::read(&path).unwrap(), after_first);
    }

    /// A row id no lane produces is a typed absence, and nothing is written.
    #[test]
    fn stamping_an_unknown_row_refuses_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let before = std::fs::read(&path).unwrap();

        let error = stamp_group(
            &root,
            "transcript_edge:nope:deadbeef:0",
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();

        assert_eq!(
            error.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A lane that MOVED between the stamper's read and its replacement is its
    /// own diagnostic, distinct from a row that was never there.
    ///
    /// The two demand opposite operator responses - re-run preflight against
    /// the moved state, versus investigate an artifact naming a row the store
    /// does not have - so collapsing them onto one token would lose the only
    /// information that distinguishes them. Both are staleness at the backfill
    /// level; only the diagnostic tells them apart.
    #[test]
    fn a_lane_that_moves_mid_stamp_is_distinct_from_an_absent_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");

        // The absent case, for contrast on the same lane.
        let absent = stamp_group(
            &root,
            "transcript_edge:nope:deadbeef:0",
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();
        assert_eq!(
            absent.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );

        // The moved case: the rewrite rechecks the source digest immediately
        // before the rename, so a lane whose bytes no longer match abandons the
        // write. Driven directly because racing a real concurrent writer would
        // make the test timing-dependent.
        let directory = NofollowDirectory::open_existing(&root).unwrap().unwrap();
        let digest = stream_lane_nofollow(
            &directory,
            "tool.jsonl",
            OwnerSnapshotLimitsV1::default(),
            |_, _| Ok(()),
        )
        .unwrap()
        .unwrap();
        std::fs::write(&path, b"{\"moved\":true}\n").unwrap();
        let moved = transcript_edge_lane_unchanged(
            &directory,
            "tool.jsonl",
            &digest,
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();

        assert_eq!(
            moved,
            bbox_corpus_core::project_catalog_snapshot::OWNER_SOURCE_MOVED
        );
        assert_ne!(
            moved,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );
        // Abandoned, not clobbered: the concurrent writer's bytes survive.
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"moved\":true}\n");
        let _ = row_id;
    }

    /// Identity ignores `project_id` and ignores key ORDER, but not content.
    /// The key-order half is what makes every stable row id in the corpus
    /// independent of whether `serde_json`'s `preserve_order` feature happens to
    /// be unified in from elsewhere in the dependency graph.
    #[test]
    fn row_identity_excludes_project_id_and_key_order_but_not_content() {
        let plain = r#"{"source":"task:one","kind":"K","target":"task:two"}"#;
        let stamped =
            r#"{"source":"task:one","kind":"K","target":"task:two","project_id":"a1b2c3d4"}"#;
        let reordered = r#"{"target":"task:two","kind":"K","source":"task:one"}"#;
        let different = r#"{"source":"task:one","kind":"OTHER","target":"task:two"}"#;

        let identity = transcript_edge_row_identity(plain).unwrap();
        assert_eq!(transcript_edge_row_identity(stamped).unwrap(), identity);
        assert_eq!(transcript_edge_row_identity(reordered).unwrap(), identity);
        assert_ne!(transcript_edge_row_identity(different).unwrap(), identity);
    }

    /// Byte-identical rows sharing one selector are ONE observation standing for
    /// both, and the aggregate id survives the stamp that fills them in.
    ///
    /// The member commitment is what keeps the duplicate visible: two rows and
    /// one row committed differently, so a lost duplicate cannot hide behind an
    /// unchanged observation.
    #[test]
    fn duplicate_rows_aggregate_into_one_observation_that_survives_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let row = r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"}}"#;
        std::fs::write(root.join("tool.jsonl"), format!("{row}\n{row}\n")).unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.row_count, 1);
        let observation = snapshot.rows[0].clone();
        assert_eq!(selector_members(&observation).row_count, 2);

        // The same lane holding ONE copy commits differently.
        std::fs::write(root.join("tool.jsonl"), format!("{row}\n")).unwrap();
        let single =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(single.rows[0].stable_row_id, observation.stable_row_id);
        assert_ne!(
            selector_members(&single.rows[0]).commitment_sha256,
            selector_members(&observation).commitment_sha256
        );

        std::fs::write(root.join("tool.jsonl"), format!("{row}\n{row}\n")).unwrap();
        stamp_group(
            &root,
            &observation.stable_row_id,
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();

        // BOTH duplicates were stamped by the one obligation, and the aggregate
        // id is unchanged, so a crash-retry recognises its own completed work.
        let after = capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
            .unwrap();
        assert_eq!(after.rows[0].stable_row_id, observation.stable_row_id);
        assert_eq!(selector_members(&after.rows[0]).row_count, 2);
        assert_eq!(
            std::fs::read_to_string(root.join("tool.jsonl"))
                .unwrap()
                .matches(r#""project_id":"a1b2c3d4""#)
                .count(),
            2
        );
    }

    /// Write `rows` catalog-visible edge lines, each padded so a handful of
    /// rows already exceeds a small injected byte budget.
    fn write_bulky_lane(path: &Path, lane: usize, rows: usize) {
        let padding = "p".repeat(160);
        let body = (0..rows)
            .map(|index| {
                format!(
                    concat!(
                        r#"{{"source":{{"type":"task","task_id":"lane-{lane}-{index}"}},"#,
                        r#""kind":"RAN_BASH","target":{{"type":"task","task_id":"peer"}},"#,
                        r#""provenance":"explicit","confidence":"exact","#,
                        r#""metadata":{{"cwd":"/repo/lane-{lane}/{index}","pad":"{padding}"}}}}"#,
                        "\n",
                    ),
                    // Named explicitly: a format string expanded from `concat!`
                    // cannot capture from the environment.
                    lane = lane,
                    index = index,
                    padding = padding,
                )
            })
            .collect::<String>();
        std::fs::write(path, body).unwrap();
    }

    /// A small stand-in for the shipped 16 MiB buffered default, so the fixture
    /// can stay a few hundred KiB while being exactly the same shape.
    fn tiny_buffered_budget() -> OwnerSnapshotLimitsV1 {
        OwnerSnapshotLimitsV1 {
            max_source_bytes: 32 * 1024,
            ..OwnerSnapshotLimitsV1::default()
        }
    }

    /// THE F3 SHAPE, which no fixture covered: a lane TREE bigger than the
    /// buffered byte budget.
    ///
    /// The budget was spent cumulatively across the whole walk, so on a real
    /// host the FIRST multi-hundred-megabyte lane exhausted it, every read came
    /// back empty, and the preflight reported a perfectly healthy host as
    /// `owner_source_unreadable`. The first assertion pins that the buffered
    /// walk really does behave that way on this fixture, so the test cannot
    /// quietly stop reproducing the defect; the second pins that this owner no
    /// longer goes through it.
    #[test]
    fn a_lane_tree_over_the_buffered_budget_captures_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        for lane in 0..3 {
            write_bulky_lane(&root.join(format!("lane-{lane}.jsonl")), lane, 200);
        }
        let limits = tiny_buffered_budget();

        let buffered = bbox_corpus_core::project_catalog_snapshot::capture_regular_tree_nofollow(
            &root,
            "transcript_edge",
            limits,
            |relative| relative.extension().and_then(|ext| ext.to_str()) == Some("jsonl"),
        )
        .unwrap();
        assert!(
            buffered
                .iter()
                .all(|(_, captured)| captured.bytes.is_none()),
            "the buffered lane must still refuse this fixture, or the test has \
             stopped reproducing the defect"
        );

        let snapshot = capture_project_catalog_owner_snapshot(&root, limits).unwrap();
        assert!(matches!(
            snapshot.state,
            OwnerSnapshotStateV1::Present { .. }
        ));
        assert_eq!(snapshot.row_count, 600);
        assert_eq!(snapshot.subsources.len(), 3);
        assert!(snapshot.rows.iter().any(|row| matches!(
            &row.value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector { literal_selector, .. }
                if literal_selector == "/repo/lane-2/199"
        )));
    }

    /// ONE lane larger than the whole buffered budget: the streamed digest is
    /// the same commitment a whole-file read would have produced, and the id the
    /// capture minted is the one the apply half resolves - which is the only
    /// reason capture and stamp are two halves of one backfill.
    #[test]
    fn one_oversized_lane_streams_the_whole_file_digest_and_stampable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tool.jsonl");
        write_bulky_lane(&path, 0, 500);
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > tiny_buffered_budget().max_source_bytes);

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, tiny_buffered_budget()).unwrap();
        assert_eq!(snapshot.row_count, 500);
        assert_eq!(
            snapshot.subsources[0].state,
            OwnerSnapshotStateV1::Present {
                content_sha256: bbox_corpus_core::project_catalog_snapshot::sha256_hex(&bytes),
                byte_len: bytes.len() as u64,
            }
        );

        // The lane is comfortably inside the DEFAULT buffered budget, so the
        // stamper resolves this streamed id by re-walking the lane it names.
        let row_id = snapshot
            .rows
            .iter()
            .find(|row| {
                matches!(
                    &row.value,
                    OwnerSnapshotRowValueV1::LegacyProjectSelector { literal_selector, .. }
                        if literal_selector == "/repo/lane-0/17"
                )
            })
            .expect("the streamed capture emits a row for every literal cwd")
            .stable_row_id
            .clone();
        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", OwnerSnapshotLimitsV1::default()).unwrap(),
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1::Stamped
        );
    }

    /// Streaming must not soften the failure mode it was introduced to stop
    /// misreporting: a lane that genuinely cannot be read is still
    /// `owner_source_unreadable`.
    ///
    /// Skips itself where the process can read a `0o000` file anyway (running
    /// privileged), because there is no unprivileged way to stage a real read
    /// failure there.
    #[cfg(unix)]
    #[test]
    fn a_lane_that_cannot_be_read_is_still_owner_source_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tool.jsonl");
        write_bulky_lane(&path, 0, 2);

        let restore = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = std::fs::File::open(&path).is_err();
        let snapshot = blocked.then(|| {
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
        });
        // Restored before any assertion so a failure cannot leave the fixture
        // undeletable.
        std::fs::set_permissions(&path, restore).unwrap();

        let Some(snapshot) = snapshot else {
            eprintln!("skipped: this process can read a 0o000 file");
            return;
        };
        assert!(matches!(
            snapshot.unwrap().state,
            OwnerSnapshotStateV1::Corrupt { diagnostic_code, .. }
                if diagnostic_code == "owner_source_unreadable"
        ));
    }

    /// Write a lane whose `rows` rows cycle through `selectors`, padded so a
    /// handful of rows already exceeds a small injected byte budget.
    fn write_selector_lane(path: &Path, selectors: &[&str], rows: usize) {
        let padding = "p".repeat(160);
        let body = (0..rows)
            .map(|index| {
                format!(
                    concat!(
                        r#"{{"source":{{"type":"task","task_id":"row-{index}"}},"#,
                        r#""kind":"RAN_BASH","target":{{"type":"task","task_id":"peer"}},"#,
                        r#""provenance":"explicit","confidence":"exact","#,
                        r#""metadata":{{"cwd":"{cwd}","pad":"{padding}"}}}}"#,
                        "\n",
                    ),
                    index = index,
                    cwd = selectors[index % selectors.len()],
                    padding = padding,
                )
            })
            .collect::<String>();
        std::fs::write(path, body).unwrap();
    }

    /// THE F8 SHAPE: millions of rows over a handful of selectors collapse to
    /// one observation per (lane, selector), carrying the count and an ordered
    /// commitment instead of the rows themselves.
    ///
    /// A per-row ledger of a real host's edge lanes is a few million entries
    /// against a canonical inventory that holds a hundred thousand in total, so
    /// this is not a size optimization: it is the only shape that fits.
    #[test]
    fn selectors_aggregate_per_lane_with_counts_and_an_ordered_commitment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        write_selector_lane(&root.join("a.jsonl"), &["/repo/one", "/repo/two"], 400);
        write_selector_lane(&root.join("b.jsonl"), &["/repo/one"], 30);

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        // Two lanes, three (lane, selector) pairs, 430 member rows.
        assert_eq!(snapshot.row_count, 3);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .map(|row| selector_members(row).row_count)
                .sum::<u64>(),
            430
        );
        // The SAME selector in two lanes is two observations, because an
        // obligation must never span lanes: one lane is one atomic write.
        let one_lane_a = snapshot
            .rows
            .iter()
            .filter(|row| {
                matches!(&row.value, OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector, ..
            } if literal_selector == "/repo/one")
            })
            .collect::<Vec<_>>();
        assert_eq!(one_lane_a.len(), 2);
        assert_ne!(one_lane_a[0].stable_row_id, one_lane_a[1].stable_row_id);

        // Deterministic over identical content, and moves with membership: a
        // dropped member cannot hide behind an unchanged observation.
        let replay =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(replay.rows, snapshot.rows);
        write_selector_lane(&root.join("b.jsonl"), &["/repo/one"], 29);
        let shrunk =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_ne!(shrunk.rows, snapshot.rows);
        assert_ne!(shrunk.canonical_sha256, snapshot.canonical_sha256);
    }

    /// F7: the apply half streams too. A lane far past the buffered byte budget
    /// is stamped in full, every unrelated line survives byte for byte, and a
    /// second apply writes nothing at all.
    #[test]
    fn a_lane_over_the_buffered_budget_stamps_streaming_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tool.jsonl");
        write_selector_lane(&path, &["/repo/one", "/repo/two"], 500);
        let limits = tiny_buffered_budget();
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.len() > limits.max_source_bytes);

        let row_id = capture_project_catalog_owner_snapshot(&root, limits)
            .unwrap()
            .rows
            .iter()
            .find(|row| {
                matches!(&row.value, OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector, ..
            } if literal_selector == "/repo/one")
            })
            .expect("the oversized lane still yields its selectors")
            .stable_row_id
            .clone();

        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", limits).unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.lines().count(), before.lines().count());
        // Exactly the 250 rows carrying that selector were stamped, and the
        // rows carrying the other selector were copied through untouched.
        assert_eq!(after.matches(r#""project_id":"a1b2c3d4""#).count(), 250);
        for (stamped_line, original) in after.lines().zip(before.lines()) {
            if original.contains(r#""cwd":"/repo/two""#) {
                assert_eq!(stamped_line, original);
            }
        }

        // The second apply of a completed obligation writes NOTHING.
        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", limits).unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
    }

    /// The verify half answers per selector group, and answers only what the
    /// group uniformly carries.
    #[test]
    fn the_batched_read_answers_per_selector_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        write_selector_lane(&root.join("tool.jsonl"), &["/repo/one", "/repo/two"], 6);
        let limits = OwnerSnapshotLimitsV1::default();
        let one = only_row_id(&root, "/repo/one");
        let two = only_row_id(&root, "/repo/two");
        stamp_group(&root, &one, "a1b2c3d4", limits).unwrap();

        let requested = read_request(&root, &[&one, &two, "transcript_edge:nope:0"], limits);
        let batch = read_project_catalog_owner_rows(&root, &requested, limits).unwrap();

        assert_eq!(
            batch.get(&one),
            Some(
                &bbox_corpus_core::project_catalog_snapshot::OwnerRowProjectIdV1::Stamped(
                    "a1b2c3d4".to_string()
                )
            )
        );
        assert_eq!(
            batch.get(&two),
            Some(&bbox_corpus_core::project_catalog_snapshot::OwnerRowProjectIdV1::Unstamped)
        );
        // An id no lane holds is absent from the batch, never a default answer.
        assert_eq!(batch.get("transcript_edge:nope:0"), None);

        // A group that is only PARTLY stamped is not a stamped group, or a torn
        // apply would verify as complete. Membership is untouched here (the row
        // identity excludes `project_id`), so this is the fold's job, not the
        // evidence check's.
        let path = root.join("tool.jsonl");
        let unstamped_first = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                if index == 0 {
                    value.as_object_mut().unwrap().remove("project_id");
                }
                format!("{}\n", serde_json::to_string(&value).unwrap())
            })
            .collect::<String>();
        std::fs::write(&path, unstamped_first).unwrap();
        let batch = read_project_catalog_owner_rows(&root, &requested, limits).unwrap();
        assert_eq!(
            batch.get(&one),
            Some(&bbox_corpus_core::project_catalog_snapshot::OwnerRowProjectIdV1::Unstamped)
        );

        // S3(e): a member appended AFTER the plan was reviewed changes the
        // group. Verify must refuse rather than answer for the set it happens
        // to find now.
        let grown = format!(
            "{}{}\n",
            std::fs::read_to_string(&path).unwrap(),
            r#"{"source":{"type":"task","task_id":"late"},"kind":"RAN_BASH","target":{"type":"task","task_id":"peer"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"}}"#
        );
        std::fs::write(&path, grown).unwrap();
        assert_eq!(
            read_project_catalog_owner_rows(&root, &requested, limits)
                .unwrap_err()
                .code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_MEMBERS_MOVED
        );
    }

    /// S3: capture records a member count and an ordered commitment so a
    /// dropped, duplicated, or substituted member is DETECTABLE, and the stamp
    /// refolds both from the walk that is about to write.
    ///
    /// Without the refold the evidence is inert: any of these three mutations
    /// leaves the surviving members uniformly stamped, so both the apply and
    /// the verify would report success over a set nobody reviewed.
    ///
    /// Each case reuses the evidence captured BEFORE the mutation, which is
    /// exactly what a plan carries.
    #[test]
    fn a_selector_group_whose_members_moved_refuses_before_writing() {
        let limits = OwnerSnapshotLimitsV1::default();
        let extra_member = concat!(
            r#"{"source":{"type":"task","task_id":"late"},"kind":"RAN_BASH","#,
            r#""target":{"type":"task","task_id":"peer"},"provenance":"explicit","#,
            r#""confidence":"exact","metadata":{"cwd":"/repo/one"}}"#,
            "\n",
        );

        // (a) a member removed, (b) a member duplicated, (c) a member
        // substituted for a different row carrying the SAME selector.
        for mutate in [
            &(|body: &str| body.lines().skip(2).collect::<Vec<_>>().join("\n") + "\n")
                as &dyn Fn(&str) -> String,
            &|body: &str| format!("{body}{}", body.lines().next().unwrap()) + "\n",
            &|body: &str| {
                let kept = body.lines().skip(2).collect::<Vec<_>>().join("\n");
                format!("{extra_member}{kept}\n")
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap().join("edges");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("tool.jsonl");
            write_selector_lane(&path, &["/repo/one", "/repo/two"], 6);

            let row_id = only_row_id(&root, "/repo/one");
            // The evidence the plan was reviewed against, taken before the
            // lane moves under it.
            let reviewed = captured_members(&root, &row_id, limits);

            let mutated = mutate(&std::fs::read_to_string(&path).unwrap());
            std::fs::write(&path, &mutated).unwrap();

            let error =
                stamp_project_catalog_owner_row(&root, &row_id, &reviewed, "a1b2c3d4", limits)
                    .unwrap_err();
            assert_eq!(
                error.code,
                bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_MEMBERS_MOVED
            );
            // BEFORE writing: the lane is exactly what the mutation left, and
            // no stamp temporary survives the refusal.
            assert_eq!(std::fs::read_to_string(&path).unwrap(), mutated);
            assert!(
                !std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("project_id"),
                "a refused stamp must not have written a project id"
            );
            let leftovers = std::fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".stamp.tmp."))
                .collect::<Vec<_>>();
            assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        }
    }

    /// S3(d): the unchanged lane is the control. Its group stamps, and the
    /// verify that follows reads it clean against the same evidence.
    #[test]
    fn an_unchanged_selector_group_stamps_and_verifies_against_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        write_selector_lane(&root.join("tool.jsonl"), &["/repo/one", "/repo/two"], 6);
        let limits = OwnerSnapshotLimitsV1::default();
        let row_id = only_row_id(&root, "/repo/one");
        let reviewed = captured_members(&root, &row_id, limits);
        assert_eq!(reviewed.row_count, 3);

        assert_eq!(
            stamp_project_catalog_owner_row(&root, &row_id, &reviewed, "a1b2c3d4", limits).unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        // The stamp did not move the evidence: identity excludes `project_id`
        // and the rewrite replaced lines in place, so the SAME reviewed value
        // still describes the group. That is what keeps a crash retry
        // idempotent instead of refusing its own completed work.
        assert_eq!(captured_members(&root, &row_id, limits), reviewed);
        assert_eq!(
            stamp_project_catalog_owner_row(&root, &row_id, &reviewed, "a1b2c3d4", limits).unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );

        let requested = [(row_id.clone(), reviewed)]
            .into_iter()
            .collect::<bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1>();
        assert_eq!(
            read_project_catalog_owner_rows(&root, &requested, limits)
                .unwrap()
                .get(&row_id),
            Some(
                &bbox_corpus_core::project_catalog_snapshot::OwnerRowProjectIdV1::Stamped(
                    "a1b2c3d4".to_string()
                )
            )
        );
    }

    /// R3-1, from the owner's side: a REAL group of three lane rows, and what
    /// its pre-evidence ledger record can and cannot say about them.
    ///
    /// The compatibility decoder reconstructs absent member evidence as a
    /// singleton, which is right for every owner whose binding was one row. It
    /// is wrong here and only here: this binding names a selector group, the
    /// group has three members, and nothing in the record says so. Writing
    /// "one row" into the migrated ledger would make every later refold
    /// disagree with it forever, on a record that is already durable, so the
    /// decode refuses instead. The second half shows the record that DOES work,
    /// and that its evidence is exactly what a fresh walk refolds.
    #[test]
    fn a_pre_evidence_ledger_record_for_a_three_row_group_cannot_be_reconstructed() {
        use bbox_corpus_core::project_catalog::decode_attachment_snapshot;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        // Six rows alternating two selectors: THREE of them carry /repo/one.
        write_selector_lane(&root.join("tool.jsonl"), &["/repo/one", "/repo/two"], 6);
        let limits = OwnerSnapshotLimitsV1::default();
        let row_id = only_row_id(&root, "/repo/one");
        let captured = captured_members(&root, &row_id, limits);
        assert_eq!(
            captured.row_count, 3,
            "the fixture must be a genuine multi-row group"
        );

        let ledger_snapshot = |evidence: &str| {
            format!(
                r#"{{
  "version": 1,
  "epoch": 4,
  "attachments": {{}},
  "scope_migration_proofs": {{}},
  "legacy_path_bindings": {{
    "lpb_11111111111111111111111111111111": {{
      "legacy_path_binding_id": "lpb_11111111111111111111111111111111",
      "historical_path": "/repo/one",
      "source_store": "transcript-edge",
      "source_row_id": "{row_id}",
{evidence}      "inventory_epoch": 3,
      "status": {{
        "kind": "unscoped"
      }}
    }}
  }}
}}
"#
            )
        };

        // Pre-evidence: the record predates the count and the commitment, and
        // no rule outside the owner's own walk can supply them.
        let error = decode_attachment_snapshot(ledger_snapshot("").as_bytes()).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_legacy_evidence_unreconstructable"
        );
        assert!(
            error
                .to_string()
                .contains("re-run the project-catalog migration"),
            "the refusal must name a repair that works: {error}"
        );

        // The record a current capture writes: it decodes, and the evidence it
        // carries is exactly what a fresh walk of the same lane refolds, which
        // is the property the whole ledger round trip rests on.
        let written = ledger_snapshot(&format!(
            "      \"member_row_count\": {},\n      \"member_commitment_sha256\": \"{}\",\n",
            captured.row_count, captured.commitment_sha256
        ));
        let snapshot = decode_attachment_snapshot(written.as_bytes()).unwrap();
        let binding = snapshot
            .legacy_path_bindings
            .values()
            .next()
            .expect("the written record decodes");
        assert_eq!(binding.member_row_count, 3);
        assert_eq!(
            binding.member_commitment_sha256,
            captured_members(&root, &row_id, limits).commitment_sha256
        );
    }

    /// THE CRASH SHAPE. A temporary left behind by an interrupted stamp is not a
    /// lane: capture must not read it, and the next apply must not trip on it.
    ///
    /// The temporary deliberately does not end in `.jsonl`, which is what keeps
    /// a crashed rewrite from presenting a half-written duplicate of a lane to
    /// the very capture the plan is derived from.
    #[test]
    fn an_abandoned_stamp_temporary_does_not_corrupt_a_re_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let path = lane_fixture(&root);
        let limits = OwnerSnapshotLimitsV1::default();
        let before = capture_project_catalog_owner_snapshot(&root, limits).unwrap();

        // Exactly what a crash between the temporary's creation and its rename
        // leaves on disk: a partial copy of the lane under the stamper's
        // temporary name.
        let body = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            root.join("tool.jsonl.stamp.tmp.99999.7"),
            &body[..body.len() / 2],
        )
        .unwrap();

        let after = capture_project_catalog_owner_snapshot(&root, limits).unwrap();
        assert_eq!(after.rows, before.rows);
        assert_eq!(after.canonical_sha256, before.canonical_sha256);

        // And the retry still applies against the real lane.
        let row_id = only_row_id(&root, "/repo/one");
        assert_eq!(
            stamp_group(&root, &row_id, "a1b2c3d4", limits).unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );
        assert!(root.join("tool.jsonl.stamp.tmp.99999.7").exists());
    }

    /// A refused stamp leaves no temporary of its own behind.
    #[test]
    fn a_refused_stamp_cleans_up_its_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        lane_fixture(&root);
        let limits = OwnerSnapshotLimitsV1::default();
        let row_id = only_row_id(&root, "/repo/one");
        stamp_group(&root, &row_id, "a1b2c3d4", limits).unwrap();

        // A conflicting re-stamp refuses mid-stream, after the temporary exists.
        stamp_group(&root, &row_id, "99999999", limits).unwrap_err();

        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".stamp.tmp."))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn legacy_compaction_rejects_project_id_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let edges = root.join("edges");
        std::fs::create_dir(&edges).unwrap();
        let outside = root.join("escape.jsonl");
        std::fs::write(&outside, b"sentinel\n").unwrap();

        let error = compact_legacy_sidecar(&edges, "../escape", true)
            .unwrap_err()
            .to_string();

        assert!(error.contains("validating edge sidecar project id"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel\n");
    }

    #[test]
    fn legacy_compaction_preserves_a_complete_backup_without_live_absence() {
        let dir = tempfile::tempdir().unwrap();
        let edges = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir(&edges).unwrap();
        let project_id = "a1b2c3d4";
        let path = edges.join(format!("{project_id}.jsonl"));
        let explicit = Edge {
            source: EntityRef::parse("task:one").unwrap(),
            kind: "RELATED_TO".into(),
            target: EntityRef::parse("task:two").unwrap(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        };
        let derived = Edge {
            provenance: EdgeProvenance::Derived,
            ..explicit.clone()
        };
        let original = format!(
            "{}\n{}\n",
            serde_json::to_string(&explicit).unwrap(),
            serde_json::to_string(&derived).unwrap()
        );
        std::fs::write(&path, &original).unwrap();

        let stats = compact_legacy_sidecar(&edges, project_id, true).unwrap();
        assert!(stats.applied);
        assert!(path.is_file());
        let backup = stats
            .backup_path
            .expect("applied compaction keeps a backup");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
        let live = std::fs::read_to_string(path).unwrap();
        assert!(live.contains("RELATED_TO"));
        assert!(!live.contains("\"provenance\":\"derived\""));
    }
}

/// Capture and stamping share ONE durable-lane population: top-level
/// lanes and observed/ are owner rows, derived/ and materialized/ are
/// rebuildable caches that must be INVISIBLE here, or a working host's
/// hundred-gigabyte cache tree blows the streaming budget and every
/// re-materialization reads as the owner moving.
#[test]
fn durable_lanes_are_an_allow_list_not_a_deny_list() {
    use std::path::Path;
    // The three live lane layouts the edge store writes.
    assert!(durable_lane(Path::new("01c2a342.jsonl")));
    assert!(durable_lane(Path::new("observed/01c2a342.jsonl")));
    assert!(durable_lane(Path::new("explicit/01c2a342.jsonl")));
    // Rebuildable caches.
    assert!(!durable_lane(Path::new("derived/01c2a342.jsonl")));
    assert!(!durable_lane(Path::new(
        "materialized/workspace/x/edges.jsonl"
    )));
    // Quarantine records are QuarantineLine rows, not edges: decoding one as
    // a lane marked the whole owner corrupt.
    assert!(!durable_lane(Path::new(
        "quarantine/01c2a342/1700000000.jsonl"
    )));
    // Retained migration staging artifacts parse as edges but are
    // point-in-time records, not live rows to count or rewrite.
    assert!(!durable_lane(Path::new(
        "migrations/mig-1/staging/explicit.jsonl"
    )));
    assert!(!durable_lane(Path::new(
        "migrations/mig-1/staging/observed.jsonl"
    )));
    // A future family must opt in by name, not inherit owner status.
    assert!(!durable_lane(Path::new("some-new-family/01c2a342.jsonl")));
    assert!(!durable_lane(Path::new("observed/deeper/01c2a342.jsonl")));
    assert!(!durable_lane(Path::new("manifest-index.json")));
}

/// The tree-level half of the allow-list pin: a tree carrying an ordinary
/// sidecar-migration state (quarantine records plus retained staging
/// artifacts, both decodable trouble if read as lanes) captures and
/// enumerates exactly the live lane through BOTH halves of the shared
/// predicate.
#[test]
fn quarantine_and_migration_staging_are_invisible_to_capture_and_stamping() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("edges");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("tool.jsonl"),
        concat!(
            r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"}}"#,
            "
"
        ),
    )
    .unwrap();
    let limits = OwnerSnapshotLimitsV1::default();
    let before = capture_project_catalog_owner_snapshot(&root, limits).unwrap();
    let lanes_before = transcript_edge_lane_set(&root, limits).unwrap();

    // A quarantined row: QuarantineLine shape, not an edge.
    std::fs::create_dir_all(root.join("quarantine/01c2a342")).unwrap();
    std::fs::write(
        root.join("quarantine/01c2a342/1700000000.jsonl"),
        concat!(
            r#"{"line":"{not-an-edge","error":"decode failure","offset":7}"#,
            "
"
        ),
    )
    .unwrap();
    // Retained migration staging: rows that DO parse as edges.
    std::fs::create_dir_all(root.join("migrations/mig-1/staging")).unwrap();
    std::fs::write(
        root.join("migrations/mig-1/staging/explicit.jsonl"),
        concat!(
            r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"}}"#,
            "
"
        ),
    )
    .unwrap();

    let after = capture_project_catalog_owner_snapshot(&root, limits).unwrap();
    assert_eq!(after.rows, before.rows);
    assert_eq!(after.canonical_sha256, before.canonical_sha256);
    let lanes_after = transcript_edge_lane_set(&root, limits).unwrap();
    assert_eq!(lanes_after, lanes_before);
}
