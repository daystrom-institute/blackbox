//! Strict migration snapshot of edge workspace manifests and selectors.
//!
//! The capture runs behind the manifest owner's coordinator and performs no
//! create, repair, activation, or fallback-to-empty behavior.

use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use crate::manifest::{
    MANIFEST_VERSION, ManifestIndex, WorkspaceManifest, manifest_index_path, materialized_dir,
};
use crate::snapshot::with_manifest_coordinator;

const SNAPSHOT_VERSION_V1: u32 = 1;
const SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.schema.v1\0";
const ROW_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.workspace-rows.v1\0";
const SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.source.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeMigrationSnapshotLimitsV1 {
    pub max_workspaces: usize,
    pub max_source_file_bytes: u64,
    pub max_total_string_bytes: usize,
}

impl Default for EdgeMigrationSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_workspaces: 1_000_000,
            max_source_file_bytes: 16 * 1024 * 1024,
            max_total_string_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeMigrationSourceStateV1 {
    Present,
    Missing,
    Corrupt { diagnostic_code: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeWorkspaceMigrationRowV1 {
    pub workspace_id: String,
    pub project_id: String,
    pub manifest_source_fingerprint_sha256: String,
    pub repo_id: Option<String>,
    pub head_sha: Option<String>,
    pub active_snapshot_id: Option<String>,
    pub active_dirty_overlay_id: Option<String>,
    pub active_snapshot_path: Option<String>,
    pub dirty_overlay_path: Option<String>,
    pub repo_materialization: Option<String>,
    pub code_source_selector: Option<String>,
    pub code_source_generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeMigrationSnapshotV1 {
    pub version: u32,
    pub state: EdgeMigrationSourceStateV1,
    pub schema_version: u32,
    pub schema_fingerprint_sha256: String,
    pub source_fingerprint_sha256: Option<String>,
    pub workspace_count: u64,
    pub active_selector_count: u64,
    pub row_commitment_sha256: String,
    pub workspaces: Vec<EdgeWorkspaceMigrationRowV1>,
}

/// Capture the manifest index and every referenced workspace manifest under
/// the same coordinator used by the owner write paths.
pub fn capture_migration_snapshot_no_create(
    edges_dir: &Path,
    limits: EdgeMigrationSnapshotLimitsV1,
) -> EdgeMigrationSnapshotV1 {
    match with_manifest_coordinator(|| Ok(capture_locked(edges_dir, limits))) {
        Ok(snapshot) => snapshot,
        Err(_) => corrupt_snapshot("edge_manifest_coordinator_unavailable"),
    }
}

fn capture_locked(
    edges_dir: &Path,
    limits: EdgeMigrationSnapshotLimitsV1,
) -> EdgeMigrationSnapshotV1 {
    if !strict_absolute_path(edges_dir) {
        return corrupt_snapshot("edge_manifest_root_path_invalid");
    }
    match fs::symlink_metadata(edges_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_snapshot(),
        Err(_) => return corrupt_snapshot("edge_manifest_root_metadata_unreadable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_snapshot("edge_manifest_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_snapshot("edge_manifest_root_not_directory");
        }
        Ok(_) => {}
    }
    let materialized = materialized_dir(edges_dir);
    match fs::symlink_metadata(&materialized) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_snapshot(),
        Err(_) => return corrupt_snapshot("edge_materialized_metadata_unreadable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_snapshot("edge_materialized_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_snapshot("edge_materialized_root_not_directory");
        }
        Ok(_) => {}
    }

    let index_path = manifest_index_path(edges_dir);
    let index_bytes = match read_regular_bounded(&index_path, limits.max_source_file_bytes) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return missing_snapshot(),
        Err(code) => return corrupt_snapshot(code),
    };
    let index: ManifestIndex = match serde_json::from_slice::<ManifestIndex>(&index_bytes) {
        Ok(index) if index.version == MANIFEST_VERSION => index,
        _ => return corrupt_snapshot("edge_manifest_index_decode_failed"),
    };
    if index.workspaces.len() > limits.max_workspaces {
        return corrupt_snapshot("edge_manifest_workspace_limit");
    }

    let mut source = Sha256::new();
    source.update(SOURCE_HASH_DOMAIN);
    hash_field(&mut source, &index_bytes);
    let mut workspaces = Vec::with_capacity(index.workspaces.len());
    let mut total_string_bytes = 0usize;
    for (project_id, entry) in index.workspaces {
        if !valid_token(&project_id) {
            return corrupt_snapshot("edge_manifest_project_id_invalid");
        }
        let relative_manifest = Path::new(&entry.manifest);
        if !strict_relative_path(relative_manifest)
            || path_has_symlink(&materialized, relative_manifest)
        {
            return corrupt_snapshot("edge_workspace_manifest_path_unsafe");
        }
        let manifest_path = materialized.join(relative_manifest);
        let manifest_bytes =
            match read_regular_bounded(&manifest_path, limits.max_source_file_bytes) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return corrupt_snapshot("edge_workspace_manifest_missing"),
                Err(code) => return corrupt_snapshot(code),
            };
        let manifest: WorkspaceManifest =
            match serde_json::from_slice::<WorkspaceManifest>(&manifest_bytes) {
                Ok(manifest)
                    if manifest.version == MANIFEST_VERSION
                        && manifest.project_id == project_id =>
                {
                    manifest
                }
                _ => return corrupt_snapshot("edge_workspace_manifest_decode_failed"),
            };
        if !optional_token(&manifest.repo_id)
            || !optional_token(&manifest.active_snapshot_id)
            || !optional_token(&manifest.active_dirty_overlay_id)
            || !optional_relative_path(&entry.active_snapshot)
            || !optional_relative_path(&entry.dirty_overlay)
            || !optional_relative_path(&entry.repo_materialization)
            || !optional_token(&entry.code_source_selector)
            || !optional_token(&entry.code_source_generation)
            || manifest
                .head_sha
                .as_deref()
                .is_some_and(|sha| !valid_commit_sha(sha))
            || entry.active_snapshot.is_some() != manifest.active_snapshot_id.is_some()
            || entry.dirty_overlay.is_some() != manifest.active_dirty_overlay_id.is_some()
            || entry.code_source_selector.is_some() != entry.code_source_generation.is_some()
        {
            return corrupt_snapshot("edge_workspace_selector_invalid");
        }
        total_string_bytes = match total_string_bytes.checked_add(
            project_id.len()
                + entry.manifest.len()
                + optional_len(&manifest.repo_id)
                + optional_len(&manifest.head_sha)
                + optional_len(&manifest.active_snapshot_id)
                + optional_len(&manifest.active_dirty_overlay_id)
                + optional_len(&entry.active_snapshot)
                + optional_len(&entry.dirty_overlay)
                + optional_len(&entry.repo_materialization)
                + optional_len(&entry.code_source_selector)
                + optional_len(&entry.code_source_generation),
        ) {
            Some(value) if value <= limits.max_total_string_bytes => value,
            _ => return corrupt_snapshot("edge_manifest_string_byte_limit"),
        };
        let manifest_fingerprint = domain_hash(
            SOURCE_HASH_DOMAIN,
            [entry.manifest.as_bytes(), manifest_bytes.as_slice()],
        );
        hash_field(&mut source, entry.manifest.as_bytes());
        hash_field(&mut source, &manifest_bytes);
        workspaces.push(EdgeWorkspaceMigrationRowV1 {
            workspace_id: project_id.clone(),
            project_id,
            manifest_source_fingerprint_sha256: manifest_fingerprint,
            repo_id: manifest.repo_id,
            head_sha: manifest.head_sha,
            active_snapshot_id: manifest.active_snapshot_id,
            active_dirty_overlay_id: manifest.active_dirty_overlay_id,
            active_snapshot_path: entry.active_snapshot,
            dirty_overlay_path: entry.dirty_overlay,
            repo_materialization: entry.repo_materialization,
            code_source_selector: entry.code_source_selector,
            code_source_generation: entry.code_source_generation,
        });
    }

    let active_selector_count = workspaces
        .iter()
        .filter(|workspace| workspace.code_source_selector.is_some())
        .count() as u64;
    let row_commitment = hash_rows(&workspaces);
    EdgeMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: EdgeMigrationSourceStateV1::Present,
        schema_version: MANIFEST_VERSION,
        schema_fingerprint_sha256: schema_fingerprint(),
        source_fingerprint_sha256: Some(hex::encode(source.finalize())),
        workspace_count: workspaces.len() as u64,
        active_selector_count,
        row_commitment_sha256: row_commitment,
        workspaces,
    }
}

fn missing_snapshot() -> EdgeMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = EdgeMigrationSourceStateV1::Missing;
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn corrupt_snapshot(code: &'static str) -> EdgeMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = EdgeMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn empty_snapshot() -> EdgeMigrationSnapshotV1 {
    EdgeMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: EdgeMigrationSourceStateV1::Present,
        schema_version: MANIFEST_VERSION,
        schema_fingerprint_sha256: schema_fingerprint(),
        source_fingerprint_sha256: Some(empty_hash(SOURCE_HASH_DOMAIN)),
        workspace_count: 0,
        active_selector_count: 0,
        row_commitment_sha256: empty_hash(ROW_HASH_DOMAIN),
        workspaces: Vec::new(),
    }
}

fn schema_fingerprint() -> String {
    domain_hash(
        SCHEMA_HASH_DOMAIN,
        [
            &MANIFEST_VERSION.to_be_bytes()[..],
            b"workspace+active_snapshot+dirty_overlay+repo_materialization+code_source_selector+code_source_generation",
        ],
    )
}

fn hash_rows(rows: &[EdgeWorkspaceMigrationRowV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(ROW_HASH_DOMAIN);
    for row in rows {
        for value in [
            Some(row.workspace_id.as_str()),
            Some(row.project_id.as_str()),
            Some(row.manifest_source_fingerprint_sha256.as_str()),
            row.repo_id.as_deref(),
            row.head_sha.as_deref(),
            row.active_snapshot_id.as_deref(),
            row.active_dirty_overlay_id.as_deref(),
            row.active_snapshot_path.as_deref(),
            row.dirty_overlay_path.as_deref(),
            row.repo_materialization.as_deref(),
            row.code_source_selector.as_deref(),
            row.code_source_generation.as_deref(),
        ] {
            hash_field(&mut digest, value.unwrap_or("").as_bytes());
        }
    }
    hex::encode(digest.finalize())
}

fn domain_hash<'a>(domain: &[u8], fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for field in fields {
        hash_field(&mut digest, field);
    }
    hex::encode(digest.finalize())
}

fn empty_hash(domain: &[u8]) -> String {
    hex::encode(Sha256::new().chain_update(domain).finalize())
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn strict_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn strict_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.to_string_lossy().contains('\\')
        && !path.to_string_lossy().chars().any(char::is_control)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn path_has_symlink(root: &Path, relative: &Path) -> bool {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return true;
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }
    false
}

fn read_regular_bounded(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, &'static str> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("edge_manifest_source_metadata_unreadable"),
    };
    if metadata.file_type().is_symlink() {
        return Err("edge_manifest_source_symlinked");
    }
    if !metadata.is_file() {
        return Err("edge_manifest_source_not_regular");
    }
    if metadata.len() > max_bytes {
        return Err("edge_manifest_source_byte_limit");
    }
    let file = fs::File::open(path).map_err(|_| "edge_manifest_source_open_failed")?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "edge_manifest_source_read_failed")?;
    if bytes.len() as u64 > max_bytes {
        return Err("edge_manifest_source_byte_limit");
    }
    Ok(Some(bytes))
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.contains(['/', '\\'])
        && !matches!(value, "." | "..")
}

fn optional_token(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(valid_token)
}

fn optional_relative_path(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_none_or(|value| strict_relative_path(Path::new(value)))
}

fn optional_len(value: &Option<String>) -> usize {
    value.as_deref().map(str::len).unwrap_or(0)
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestIndex, WorkspaceIndexEntry, WorkspaceManifest};

    #[test]
    fn missing_root_is_typed_and_never_created() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("missing");

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, EdgeMigrationSourceStateV1::Missing);
        assert!(!root.exists());
    }

    #[test]
    fn captures_complete_workspace_and_selector_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manifest = WorkspaceManifest {
            version: MANIFEST_VERSION,
            project_id: "project-a".to_string(),
            repo_id: Some("repo-a".to_string()),
            canonical_path: None,
            git_common_dir: None,
            git_worktree_dir: None,
            branch: None,
            head_sha: Some("1111111111111111111111111111111111111111".to_string()),
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("snapshot-a".to_string()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        WorkspaceManifest::write_to(&root, &manifest).unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: Some("snapshot-a".to_string()),
                dirty_overlay: None,
                repo_materialization: Some("repo-a".to_string()),
                code_source_selector: Some("selector-a".to_string()),
                code_source_generation: Some("generation-a".to_string()),
            },
        );
        index.write_atomic(&root).unwrap();

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, EdgeMigrationSourceStateV1::Present);
        assert_eq!(snapshot.workspace_count, 1);
        assert_eq!(snapshot.active_selector_count, 1);
        assert_eq!(
            snapshot.workspaces[0].code_source_selector.as_deref(),
            Some("selector-a")
        );
    }

    #[test]
    fn missing_referenced_workspace_is_corrupt_not_empty() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
            },
        );
        index.write_atomic(&root).unwrap();

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert!(matches!(
            snapshot.state,
            EdgeMigrationSourceStateV1::Corrupt {
                diagnostic_code: "edge_workspace_manifest_missing"
            }
        ));
    }
}
