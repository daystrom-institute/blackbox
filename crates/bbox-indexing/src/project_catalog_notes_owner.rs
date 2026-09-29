//! The project-catalog inventory owner for per-repository `provenance` Git
//! notes.
//!
//! Catalog migration inventories every durable owner that can carry project
//! identity, and a registered repository may hold documents under
//! `refs/notes/<namespace>/provenance`. This module validates that notes ref
//! and captures the documents as inventory-target rows. It only reads: nothing
//! in the workspace writes these notes.

use bbox_corpus_core::git::{NOTE_DOCUMENT_SEPARATOR, StableGitRepository};
use bbox_corpus_core::project_catalog_snapshot::{
    OwnerSnapshotError, OwnerSnapshotLimitsV1, OwnerSnapshotRowV1, OwnerSnapshotStateV1,
    OwnerSnapshotV1, build_owner_snapshot, corrupt_owner_snapshot, finalize_owner_snapshot,
    missing_owner_snapshot, owner_subsource,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const OWNER: &str = "provenance";
const NOTES_REF_SUBSOURCE: &str = "provenance:notes-ref";

/// `refs/notes/<safe-namespace>/provenance`, and nothing else.
pub(crate) fn validate_notes_ref(notes_ref: &str) -> anyhow::Result<()> {
    let mut components = notes_ref.split('/');
    let valid_prefix = components.next() == Some("refs") && components.next() == Some("notes");
    let namespace = components.next().unwrap_or_default();
    let valid_suffix = components.next() == Some("provenance") && components.next().is_none();
    if !valid_prefix || !valid_suffix || !is_safe_notes_namespace(namespace) {
        anyhow::bail!(
            "invalid provenance notes ref: expected refs/notes/<safe-namespace>/provenance"
        );
    }
    Ok(())
}

fn is_safe_notes_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace != "."
        && namespace != ".."
        && !namespace.starts_with('-')
        && !namespace.starts_with('.')
        && !namespace.ends_with('.')
        && !namespace.to_ascii_lowercase().ends_with(".lock")
        && !namespace.contains("..")
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The fields a note document must carry to count as well formed. Everything
/// else in the document is opaque to the inventory.
#[derive(Deserialize)]
struct NoteDocumentHeader {
    #[serde(default = "first_schema_version")]
    schema_version: u32,
    commit: String,
    #[serde(rename = "produced_by")]
    _produced_by: serde::de::IgnoredAny,
    #[serde(rename = "tool_calls")]
    _tool_calls: Vec<serde::de::IgnoredAny>,
}

const fn first_schema_version() -> u32 {
    1
}

fn parse_note_commit(document: &str) -> Option<String> {
    let header = serde_json::from_str::<NoteDocumentHeader>(document.trim()).ok()?;
    matches!(header.schema_version, 1 | 2).then_some(header.commit)
}

fn split_note_documents(raw: &str) -> Vec<&str> {
    raw.split(NOTE_DOCUMENT_SEPARATOR)
        .map(str::trim)
        .filter(|document| !document.is_empty())
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Capture every document under one explicit notes ref of one repository.
///
/// The note blobs are read by the immutable object ids returned from one
/// notes listing, so the capture cannot mix documents across ref
/// generations. Each document becomes one inventory-target row carrying the
/// caller's project id; the owner never emits a legacy selector row.
pub(crate) fn capture_owner_snapshot(
    repository: &StableGitRepository,
    notes_ref: &str,
    project_id: &str,
    limits: OwnerSnapshotLimitsV1,
) -> Result<OwnerSnapshotV1, OwnerSnapshotError> {
    if project_id.trim().is_empty() || validate_notes_ref(notes_ref).is_err() {
        return corrupt_owner_snapshot(
            OWNER,
            NOTES_REF_SUBSOURCE,
            "provenance_capture_input_invalid",
            limits,
        );
    }
    let note_entries = match repository.snapshot_notes_bounded(
        notes_ref,
        limits.max_subsources,
        limits.max_source_bytes,
    ) {
        Ok(Some(entries)) => entries,
        Ok(None) => return missing_owner_snapshot(OWNER, NOTES_REF_SUBSOURCE, limits),
        Err(_) => {
            return corrupt_owner_snapshot(
                OWNER,
                NOTES_REF_SUBSOURCE,
                "provenance_notes_snapshot_unreadable",
                limits,
            );
        }
    };
    let mut listing_commitment = Vec::new();
    for entry in &note_entries {
        listing_commitment.extend_from_slice(entry.target_oid.as_bytes());
        listing_commitment.push(0);
        listing_commitment.extend_from_slice(sha256_hex(&entry.bytes).as_bytes());
        listing_commitment.push(b'\n');
    }
    if note_entries.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(&listing_commitment),
            byte_len: 0,
        };
        return build_owner_snapshot(
            OWNER,
            vec![owner_subsource(NOTES_REF_SUBSOURCE, state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut total_bytes = listing_commitment.len();
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for entry in note_entries {
        let commit_oid = entry.target_oid;
        let bytes = entry.bytes;
        total_bytes = match total_bytes.checked_add(bytes.len()) {
            Some(total_bytes) if total_bytes <= limits.max_source_bytes => total_bytes,
            _ => {
                return corrupt_owner_snapshot(
                    OWNER,
                    NOTES_REF_SUBSOURCE,
                    "owner_source_byte_limit",
                    limits,
                );
            }
        };
        let subsource_id = format!("provenance:{project_id}:{commit_oid}");
        let Ok(body) = std::str::from_utf8(&bytes) else {
            return corrupt_owner_snapshot(OWNER, &subsource_id, "provenance_note_invalid", limits);
        };
        let mut subsource_rows = Vec::new();
        for (index, document) in split_note_documents(body).into_iter().enumerate() {
            let Some(commit) = parse_note_commit(document) else {
                return corrupt_owner_snapshot(
                    OWNER,
                    &subsource_id,
                    "provenance_note_invalid",
                    limits,
                );
            };
            if commit != commit_oid {
                return corrupt_owner_snapshot(
                    OWNER,
                    &subsource_id,
                    "provenance_note_commit_mismatch",
                    limits,
                );
            }
            let hash = sha256_hex(document.as_bytes());
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{commit_oid}:{index}:{hash}"),
                project_id,
                hash,
            ));
        }
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(&bytes),
            byte_len: bytes.len() as u64,
        };
        subsources.push(owner_subsource(subsource_id, state, &subsource_rows));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot(OWNER, NOTES_REF_SUBSOURCE, subsources, rows, limits)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotRowValueV1;

    use super::*;

    const NOTES_REF: &str = "refs/notes/bbox/provenance";

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn init_repo() -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-qm", "initial"]);
        let commit = git(&root, &["rev-parse", "HEAD"]);
        (dir, root, commit)
    }

    fn document(commit: &str, tool: &str) -> String {
        serde_json::json!({
            "schema_version": 2,
            "commit": commit,
            "produced_by": {},
            "tool_calls": [{"tool": tool}],
        })
        .to_string()
    }

    fn add_note(root: &Path, commit: &str, documents: &[String]) {
        let body = documents.join(&format!("\n{NOTE_DOCUMENT_SEPARATOR}\n"));
        git(
            root,
            &[
                "notes", "--ref", NOTES_REF, "add", "-f", "-m", &body, commit,
            ],
        );
    }

    fn capture(root: &Path, project_id: &str) -> OwnerSnapshotV1 {
        let directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(root)
            .unwrap()
            .unwrap();
        let repository = bbox_corpus_core::git::open_stable_git_repository(&directory)
            .unwrap()
            .unwrap();
        capture_owner_snapshot(
            &repository,
            NOTES_REF,
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap()
    }

    #[test]
    fn note_ref_validation_is_structurally_confined() {
        assert!(validate_notes_ref(NOTES_REF).is_ok());
        for invalid in [
            "refs/notes/../provenance",
            "refs/notes/-bbox/provenance",
            "refs/notes/.bbox/provenance",
            "refs/notes/bbox./provenance",
            "refs/notes/bbox.lock/provenance",
            "refs/notes/bbox/other",
            "refs/heads/main",
            "refs/notes/b box/provenance",
            "refs/notes/bbox/../../heads/main",
        ] {
            assert!(validate_notes_ref(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn absent_notes_ref_is_missing_and_capture_creates_nothing() {
        let (_dir, root, _commit) = init_repo();
        let snapshot = capture(&root, "project1");
        assert!(matches!(
            snapshot.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(git(&root, &["for-each-ref", "refs/notes"]).is_empty());
    }

    /// A nonempty notes corpus yields inventory-target rows only, each
    /// carrying the supplied project id: the owner can never produce a
    /// legacy selector, so it never creates a stamping obligation.
    #[test]
    fn nonempty_notes_yield_only_inventory_targets_carrying_the_project_id() {
        let (_dir, root, commit) = init_repo();
        add_note(
            &root,
            &commit,
            &[document(&commit, "Edit"), document(&commit, "Read")],
        );

        let snapshot = capture(&root, "project1");
        assert_eq!(snapshot.row_count, 2, "expected a nonempty two-row corpus");
        for row in &snapshot.rows {
            match &row.value {
                OwnerSnapshotRowValueV1::InventoryTarget {
                    project_id,
                    target_sha256,
                } => {
                    assert_eq!(project_id, "project1");
                    assert_eq!(target_sha256.len(), 64);
                }
                OwnerSnapshotRowValueV1::LegacyProjectSelector { .. } => {
                    panic!("the notes owner emitted a legacy selector")
                }
            }
        }
    }

    #[test]
    fn capture_requires_a_project_id() {
        let (_dir, root, commit) = init_repo();
        add_note(&root, &commit, &[document(&commit, "Edit")]);
        assert!(matches!(
            capture(&root, "   ").state,
            OwnerSnapshotStateV1::Corrupt { .. }
        ));
    }

    #[test]
    fn a_document_naming_another_commit_is_corrupt() {
        let (_dir, root, commit) = init_repo();
        add_note(&root, &commit, &[document(&"1".repeat(40), "Edit")]);
        assert!(matches!(
            capture(&root, "project1").state,
            OwnerSnapshotStateV1::Corrupt { .. }
        ));
    }
}
