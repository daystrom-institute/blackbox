//! Bounded, read-only migration inventory for vector partitions.
//!
//! The root capture path never opens the create-on-read `VectorStore` API.
//! It understands the private WAL/snapshot formats here, in their owner
//! crate, and exposes only entity keys and commitments, never vector bodies.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use super::{
    PartitionSnapshot, VECTOR_SCHEMA_VERSION, VECTOR_SNAPSHOT_VERSION, VectorStore, read_snapshot,
};
use crate::wal::WalRecord;

const SNAPSHOT_VERSION_V1: u32 = 1;
const SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.vectors.schema.v1\0";
const PARTITION_ROW_HASH_DOMAIN: &[u8] = b"blackbox.vectors.partition-keys.v1\0";
const PROJECT_REF_HASH_DOMAIN: &[u8] = b"blackbox.vectors.project-refs.v1\0";
const COMMIT_NAMESPACE_HASH_DOMAIN: &[u8] = b"blackbox.vectors.commit-namespace.v1\0";
const SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.vectors.source.v1\0";
const WAL_FILE: &str = "records.wal";
const SNAPSHOT_FILE: &str = "snapshot.bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorMigrationSnapshotLimitsV1 {
    pub max_partitions: usize,
    pub max_active_keys: usize,
    pub max_partition_source_bytes: u64,
    pub max_total_string_bytes: usize,
}

impl Default for VectorMigrationSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_partitions: 1_024,
            max_active_keys: 10_000_000,
            max_partition_source_bytes: 16 * 1024 * 1024 * 1024,
            max_total_string_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorMigrationSourceStateV1 {
    Present,
    Missing,
    Corrupt { diagnostic_code: &'static str },
    Unavailable { diagnostic_code: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorPartitionMigrationSnapshotV1 {
    pub route: String,
    pub state: VectorMigrationSourceStateV1,
    pub schema_version: Option<String>,
    pub source_fingerprint_sha256: Option<String>,
    pub active_key_count: u64,
    pub active_key_commitment_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorProjectScopedRefV1 {
    pub route: String,
    pub project_id: String,
    pub entity_ref: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorCommitNamespaceV1 {
    pub namespace: String,
    pub vector_key_count: u64,
    pub vector_key_commitment_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorMigrationSnapshotV1 {
    pub version: u32,
    pub state: VectorMigrationSourceStateV1,
    pub schema_version: String,
    pub schema_fingerprint_sha256: String,
    pub source_fingerprint_sha256: Option<String>,
    pub partition_count: u64,
    pub active_key_count: u64,
    pub project_scoped_ref_count: u64,
    pub project_scoped_ref_commitment_sha256: String,
    pub partitions: Vec<VectorPartitionMigrationSnapshotV1>,
    pub project_scoped_refs: Vec<VectorProjectScopedRefV1>,
    pub commit_namespaces: Vec<VectorCommitNamespaceV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ActiveKeyV1 {
    route: String,
    entity_ref: String,
    content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedPartitionV1 {
    evidence: VectorPartitionMigrationSnapshotV1,
    active_keys: Vec<ActiveKeyV1>,
}

/// Capture the durable vector root without creating a missing root,
/// partition, derived file, or repaired snapshot.
pub fn capture_migration_snapshot_no_create(
    root: &Path,
    limits: VectorMigrationSnapshotLimitsV1,
) -> VectorMigrationSnapshotV1 {
    if !strict_absolute_path(root) {
        return corrupt_snapshot("vector_root_path_invalid");
    }
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_snapshot(),
        Err(_) => return unavailable_snapshot("vector_root_metadata_unavailable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_snapshot("vector_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_snapshot("vector_root_not_directory");
        }
        Ok(_) => {}
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return unavailable_snapshot("vector_root_unavailable"),
    };
    let mut partition_paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return unavailable_snapshot("vector_partition_entry_unavailable"),
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return unavailable_snapshot("vector_partition_entry_unavailable"),
        };
        if file_type.is_symlink() {
            return corrupt_snapshot("vector_partition_entry_symlinked");
        }
        if !file_type.is_dir() {
            continue;
        }
        let Some(route) = entry.file_name().to_str().map(str::to_string) else {
            return corrupt_snapshot("vector_partition_route_invalid");
        };
        if !valid_route(&route) {
            return corrupt_snapshot("vector_partition_route_invalid");
        }
        partition_paths.push((route, entry.path()));
    }
    partition_paths.sort_by(|left, right| left.0.cmp(&right.0));
    if partition_paths.len() > limits.max_partitions {
        return corrupt_snapshot("vector_partition_limit");
    }

    let mut captured = Vec::with_capacity(partition_paths.len());
    for (route, path) in partition_paths {
        captured.push(capture_partition_no_create(&route, &path, limits));
    }
    assemble_snapshot(captured, limits)
}

impl VectorStore {
    /// Capture the installed owner under its existing partition read locks.
    ///
    /// All partition guards remain held until every active key is copied, so
    /// a caller never receives a cross-partition mix of write epochs.
    pub fn capture_migration_snapshot(
        &self,
        limits: VectorMigrationSnapshotLimitsV1,
    ) -> VectorMigrationSnapshotV1 {
        let partitions = self.partitions.read();
        if partitions.len() > limits.max_partitions {
            return corrupt_snapshot("vector_partition_limit");
        }
        let guards = partitions
            .iter()
            .map(|(route, partition)| (route.clone(), partition.read()))
            .collect::<Vec<_>>();
        let captured = guards
            .iter()
            .map(|(route, partition)| {
                let mut active_keys = partition
                    .slab
                    .active_entries()
                    .map(|entry| ActiveKeyV1 {
                        route: route.clone(),
                        entity_ref: entry.entity_id.clone(),
                        content_hash: entry.content_hash.clone(),
                    })
                    .collect::<Vec<_>>();
                active_keys.sort();
                let commitment = hash_active_keys(&active_keys);
                CapturedPartitionV1 {
                    evidence: VectorPartitionMigrationSnapshotV1 {
                        route: route.clone(),
                        state: VectorMigrationSourceStateV1::Present,
                        schema_version: Some(VECTOR_SCHEMA_VERSION.to_string()),
                        source_fingerprint_sha256: Some(commitment.clone()),
                        active_key_count: active_keys.len() as u64,
                        active_key_commitment_sha256: commitment,
                    },
                    active_keys,
                }
            })
            .collect::<Vec<_>>();
        assemble_snapshot(captured, limits)
    }
}

fn capture_partition_no_create(
    route: &str,
    path: &Path,
    limits: VectorMigrationSnapshotLimitsV1,
) -> CapturedPartitionV1 {
    let wal_path = path.join(WAL_FILE);
    match fs::symlink_metadata(&wal_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_partition(route, "vector_partition_wal_symlinked");
        }
        Ok(metadata) if !metadata.is_file() => {
            return corrupt_partition(route, "vector_partition_wal_not_regular");
        }
        Ok(metadata) if metadata.len() > limits.max_partition_source_bytes => {
            return corrupt_partition(route, "vector_partition_source_byte_limit");
        }
        Ok(_) => return capture_partition_from_wal(route, &wal_path, limits),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return unavailable_partition(route, "vector_partition_wal_metadata_unavailable");
        }
    }

    let snapshot_path = path.join(SNAPSHOT_FILE);
    let metadata = match fs::symlink_metadata(&snapshot_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return corrupt_partition(route, "vector_partition_source_missing");
        }
        Err(_) => {
            return unavailable_partition(route, "vector_partition_snapshot_metadata_unavailable");
        }
    };
    if metadata.file_type().is_symlink() {
        return corrupt_partition(route, "vector_partition_snapshot_symlinked");
    }
    if !metadata.is_file() {
        return corrupt_partition(route, "vector_partition_snapshot_not_regular");
    }
    if metadata.len() > limits.max_partition_source_bytes {
        return corrupt_partition(route, "vector_partition_source_byte_limit");
    }
    let snapshot = match read_snapshot(&snapshot_path) {
        Ok(snapshot) => snapshot,
        Err(error)
            if error
                .chain()
                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some()) =>
        {
            return unavailable_partition(route, "vector_partition_snapshot_read_unavailable");
        }
        Err(_) => return corrupt_partition(route, "vector_partition_snapshot_decode_failed"),
    };
    capture_partition_from_snapshot(route, snapshot)
}

fn capture_partition_from_wal(
    route: &str,
    wal_path: &Path,
    limits: VectorMigrationSnapshotLimitsV1,
) -> CapturedPartitionV1 {
    let file = match fs::File::open(wal_path) {
        Ok(file) => file,
        Err(_) => return unavailable_partition(route, "vector_partition_wal_open_unavailable"),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut active = BTreeMap::<String, String>::new();
    loop {
        line.clear();
        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(_) => return unavailable_partition(route, "vector_partition_wal_read_unavailable"),
        };
        if read == 0 {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: WalRecord = match serde_json::from_slice(&line) {
            Ok(record) => record,
            Err(_) => return corrupt_partition(route, "vector_partition_wal_decode_failed"),
        };
        if record.route != route || record.model != route {
            return corrupt_partition(route, "vector_partition_wal_route_mismatch");
        }
        if record.entity_id.is_empty() || record.entity_id.len() > limits.max_total_string_bytes {
            return corrupt_partition(route, "vector_partition_entity_id_invalid");
        }
        if record.deleted_at.is_some() {
            active.remove(&record.entity_id);
            continue;
        }
        if record.content_hash.is_empty() || record.dims == 0 || record.dims != record.vector.len()
        {
            return corrupt_partition(route, "vector_partition_wal_record_invalid");
        }
        active.insert(record.entity_id, record.content_hash);
        if active.len() > limits.max_active_keys {
            return corrupt_partition(route, "vector_active_key_limit");
        }
    }
    let active_keys = active
        .into_iter()
        .map(|(entity_ref, content_hash)| ActiveKeyV1 {
            route: route.to_string(),
            entity_ref,
            content_hash,
        })
        .collect::<Vec<_>>();
    let source_fingerprint = hash_active_keys(&active_keys);
    present_partition(route, active_keys, source_fingerprint)
}

fn capture_partition_from_snapshot(
    route: &str,
    mut snapshot: PartitionSnapshot,
) -> CapturedPartitionV1 {
    if snapshot.schema_version != VECTOR_SNAPSHOT_VERSION || snapshot.route != route {
        return corrupt_partition(route, "vector_partition_snapshot_identity_mismatch");
    }
    snapshot.slab.rebuild_active_index();
    let active_keys = snapshot
        .slab
        .active_entries()
        .map(|entry| ActiveKeyV1 {
            route: route.to_string(),
            entity_ref: entry.entity_id.clone(),
            content_hash: entry.content_hash.clone(),
        })
        .collect::<Vec<_>>();
    let source_fingerprint = hash_active_keys(&active_keys);
    present_partition(route, active_keys, source_fingerprint)
}

fn present_partition(
    route: &str,
    mut active_keys: Vec<ActiveKeyV1>,
    source_fingerprint_sha256: String,
) -> CapturedPartitionV1 {
    active_keys.sort();
    let commitment = hash_active_keys(&active_keys);
    CapturedPartitionV1 {
        evidence: VectorPartitionMigrationSnapshotV1 {
            route: route.to_string(),
            state: VectorMigrationSourceStateV1::Present,
            schema_version: Some(VECTOR_SCHEMA_VERSION.to_string()),
            source_fingerprint_sha256: Some(source_fingerprint_sha256),
            active_key_count: active_keys.len() as u64,
            active_key_commitment_sha256: commitment,
        },
        active_keys,
    }
}

fn corrupt_partition(route: &str, code: &'static str) -> CapturedPartitionV1 {
    CapturedPartitionV1 {
        evidence: VectorPartitionMigrationSnapshotV1 {
            route: route.to_string(),
            state: VectorMigrationSourceStateV1::Corrupt {
                diagnostic_code: code,
            },
            schema_version: None,
            source_fingerprint_sha256: None,
            active_key_count: 0,
            active_key_commitment_sha256: empty_hash(PARTITION_ROW_HASH_DOMAIN),
        },
        active_keys: Vec::new(),
    }
}

fn unavailable_partition(route: &str, code: &'static str) -> CapturedPartitionV1 {
    CapturedPartitionV1 {
        evidence: VectorPartitionMigrationSnapshotV1 {
            route: route.to_string(),
            state: VectorMigrationSourceStateV1::Unavailable {
                diagnostic_code: code,
            },
            schema_version: None,
            source_fingerprint_sha256: None,
            active_key_count: 0,
            active_key_commitment_sha256: empty_hash(PARTITION_ROW_HASH_DOMAIN),
        },
        active_keys: Vec::new(),
    }
}

fn assemble_snapshot(
    captured: Vec<CapturedPartitionV1>,
    limits: VectorMigrationSnapshotLimitsV1,
) -> VectorMigrationSnapshotV1 {
    let schema_fingerprint = domain_hash(
        SCHEMA_HASH_DOMAIN,
        [
            VECTOR_SCHEMA_VERSION.as_bytes(),
            VECTOR_SNAPSHOT_VERSION.as_bytes(),
        ],
    );
    let any_corrupt = captured.iter().any(|partition| {
        matches!(
            partition.evidence.state,
            VectorMigrationSourceStateV1::Corrupt { .. }
        )
    });
    let unavailable = captured
        .iter()
        .find_map(|partition| match partition.evidence.state {
            VectorMigrationSourceStateV1::Unavailable { diagnostic_code } => Some(diagnostic_code),
            _ => None,
        });
    let mut all_keys = captured
        .iter()
        .flat_map(|partition| partition.active_keys.iter().cloned())
        .collect::<Vec<_>>();
    all_keys.sort();
    if all_keys.len() > limits.max_active_keys {
        return corrupt_snapshot("vector_active_key_limit");
    }
    let string_bytes = all_keys.iter().fold(0usize, |total, row| {
        total.saturating_add(row.route.len() + row.entity_ref.len() + row.content_hash.len())
    });
    if string_bytes > limits.max_total_string_bytes {
        return corrupt_snapshot("vector_string_byte_limit");
    }
    if all_keys.iter().any(|row| {
        (is_project_scoped_entity(&row.entity_ref)
            && project_id_from_entity_ref(&row.entity_ref).is_none())
            || (row.entity_ref.starts_with("commit:")
                && commit_namespace(&row.entity_ref).is_none())
    }) {
        return corrupt_snapshot("vector_entity_ref_invalid");
    }

    let project_scoped_refs = all_keys
        .iter()
        .filter_map(|row| {
            project_id_from_entity_ref(&row.entity_ref).map(|project_id| VectorProjectScopedRefV1 {
                route: row.route.clone(),
                project_id: project_id.to_string(),
                entity_ref: row.entity_ref.clone(),
                content_hash: row.content_hash.clone(),
            })
        })
        .collect::<Vec<_>>();
    let project_ref_commitment = hash_project_refs(&project_scoped_refs);

    let mut commit_rows = BTreeMap::<String, Vec<ActiveKeyV1>>::new();
    for row in &all_keys {
        if let Some(namespace) = commit_namespace(&row.entity_ref) {
            commit_rows
                .entry(namespace.to_string())
                .or_default()
                .push(row.clone());
        }
    }
    let commit_namespaces = commit_rows
        .into_iter()
        .map(|(namespace, rows)| VectorCommitNamespaceV1 {
            namespace,
            vector_key_count: rows.len() as u64,
            vector_key_commitment_sha256: hash_commit_namespace_rows(&rows),
        })
        .collect::<Vec<_>>();

    let partitions = captured
        .into_iter()
        .map(|partition| partition.evidence)
        .collect::<Vec<_>>();
    let mut source = Sha256::new();
    source.update(SOURCE_HASH_DOMAIN);
    hash_field(&mut source, schema_fingerprint.as_bytes());
    for partition in &partitions {
        hash_field(&mut source, partition.route.as_bytes());
        hash_field(
            &mut source,
            partition
                .source_fingerprint_sha256
                .as_deref()
                .unwrap_or("")
                .as_bytes(),
        );
        hash_field(
            &mut source,
            partition.active_key_commitment_sha256.as_bytes(),
        );
    }

    VectorMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: if let Some(diagnostic_code) = unavailable {
            VectorMigrationSourceStateV1::Unavailable { diagnostic_code }
        } else if any_corrupt {
            VectorMigrationSourceStateV1::Corrupt {
                diagnostic_code: "vector_partition_corrupt",
            }
        } else {
            VectorMigrationSourceStateV1::Present
        },
        schema_version: VECTOR_SCHEMA_VERSION.to_string(),
        schema_fingerprint_sha256: schema_fingerprint,
        source_fingerprint_sha256: Some(hex::encode(source.finalize())),
        partition_count: partitions.len() as u64,
        active_key_count: all_keys.len() as u64,
        project_scoped_ref_count: project_scoped_refs.len() as u64,
        project_scoped_ref_commitment_sha256: project_ref_commitment,
        partitions,
        project_scoped_refs,
        commit_namespaces,
    }
}

fn missing_snapshot() -> VectorMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = VectorMigrationSourceStateV1::Missing;
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn corrupt_snapshot(code: &'static str) -> VectorMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = VectorMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn unavailable_snapshot(code: &'static str) -> VectorMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = VectorMigrationSourceStateV1::Unavailable {
        diagnostic_code: code,
    };
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn empty_snapshot() -> VectorMigrationSnapshotV1 {
    VectorMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: VectorMigrationSourceStateV1::Present,
        schema_version: VECTOR_SCHEMA_VERSION.to_string(),
        schema_fingerprint_sha256: domain_hash(
            SCHEMA_HASH_DOMAIN,
            [
                VECTOR_SCHEMA_VERSION.as_bytes(),
                VECTOR_SNAPSHOT_VERSION.as_bytes(),
            ],
        ),
        source_fingerprint_sha256: Some(empty_hash(SOURCE_HASH_DOMAIN)),
        partition_count: 0,
        active_key_count: 0,
        project_scoped_ref_count: 0,
        project_scoped_ref_commitment_sha256: empty_hash(PROJECT_REF_HASH_DOMAIN),
        partitions: Vec::new(),
        project_scoped_refs: Vec::new(),
        commit_namespaces: Vec::new(),
    }
}

fn hash_active_keys(rows: &[ActiveKeyV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(PARTITION_ROW_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.route.as_bytes());
        hash_field(&mut digest, row.entity_ref.as_bytes());
        hash_field(&mut digest, row.content_hash.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn hash_project_refs(rows: &[VectorProjectScopedRefV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(PROJECT_REF_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.route.as_bytes());
        hash_field(&mut digest, row.project_id.as_bytes());
        hash_field(&mut digest, row.entity_ref.as_bytes());
        hash_field(&mut digest, row.content_hash.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn hash_commit_namespace_rows(rows: &[ActiveKeyV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(COMMIT_NAMESPACE_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.route.as_bytes());
        hash_field(&mut digest, row.entity_ref.as_bytes());
        hash_field(&mut digest, row.content_hash.as_bytes());
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

fn valid_route(route: &str) -> bool {
    !route.is_empty()
        && route.len() <= 256
        && !route.contains(['/', '\\'])
        && !matches!(route, "." | "..")
        && !route.chars().any(char::is_control)
}

fn project_id_from_entity_ref(entity_ref: &str) -> Option<&str> {
    let (kind, rest) = entity_ref.split_once(':')?;
    let expected_parts = match kind {
        "project_file" => 4,
        "project_file_v2" => 5,
        "symbol" => 3,
        "symbol_v2" => 4,
        _ => return None,
    };
    if rest.split(':').count() != expected_parts {
        return None;
    }
    let project_id = rest.split(':').next()?;
    valid_entity_component(project_id).then_some(project_id)
}

fn is_project_scoped_entity(entity_ref: &str) -> bool {
    entity_ref.starts_with("project_file:")
        || entity_ref.starts_with("project_file_v2:")
        || entity_ref.starts_with("symbol:")
        || entity_ref.starts_with("symbol_v2:")
}

fn commit_namespace(entity_ref: &str) -> Option<&str> {
    let rest = entity_ref.strip_prefix("commit:")?;
    let mut parts = rest.split(':');
    let namespace = parts.next()?;
    let sha = parts.next()?;
    (parts.next().is_none() && valid_entity_component(namespace) && valid_commit_sha(sha))
        .then_some(namespace)
}

fn valid_entity_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains([':', '/', '\\'])
        && !value.chars().any(char::is_control)
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

    #[test]
    fn missing_root_is_typed_and_never_created() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("missing");

        let snapshot =
            capture_migration_snapshot_no_create(&root, VectorMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, VectorMigrationSourceStateV1::Missing);
        assert!(!root.exists());
    }

    #[test]
    fn captures_project_refs_and_commit_keys_without_vectors() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = VectorStore::open(&root).unwrap();
        store
            .upsert(
                "route-a",
                "project_file:project-a:path:chunk:0",
                "content-a",
                vec![1.0, 0.0],
            )
            .unwrap();
        store
            .upsert(
                "route-a",
                "commit:legacy-namespace:1111111111111111111111111111111111111111",
                "commit-content",
                vec![0.0, 1.0],
            )
            .unwrap();
        drop(store);

        let snapshot =
            capture_migration_snapshot_no_create(&root, VectorMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, VectorMigrationSourceStateV1::Present);
        assert_eq!(snapshot.active_key_count, 2);
        assert_eq!(snapshot.project_scoped_ref_count, 1);
        assert_eq!(snapshot.commit_namespaces.len(), 1);
        assert_eq!(snapshot.commit_namespaces[0].vector_key_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_partition_is_corrupt_without_following_it() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, root.join("route-a")).unwrap();

        let snapshot =
            capture_migration_snapshot_no_create(&root, VectorMigrationSnapshotLimitsV1::default());

        assert!(matches!(
            snapshot.state,
            VectorMigrationSourceStateV1::Corrupt {
                diagnostic_code: "vector_partition_entry_symlinked"
            }
        ));
    }

    #[test]
    fn truncated_wal_and_source_limit_are_distinct_corrupt_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let partition = root.join("route-a");
        fs::create_dir(&partition).unwrap();
        fs::write(partition.join(WAL_FILE), b"{truncated").unwrap();

        let truncated =
            capture_migration_snapshot_no_create(&root, VectorMigrationSnapshotLimitsV1::default());
        assert!(matches!(
            truncated.partitions[0].state,
            VectorMigrationSourceStateV1::Corrupt {
                diagnostic_code: "vector_partition_wal_decode_failed"
            }
        ));

        let mut limits = VectorMigrationSnapshotLimitsV1::default();
        limits.max_partition_source_bytes = 1;
        let oversized = capture_migration_snapshot_no_create(&root, limits);
        assert!(matches!(
            oversized.partitions[0].state,
            VectorMigrationSourceStateV1::Corrupt {
                diagnostic_code: "vector_partition_source_byte_limit"
            }
        ));
    }
}
