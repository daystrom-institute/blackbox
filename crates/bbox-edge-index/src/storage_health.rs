use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    ActiveLegacy,
    ManagedDerived,
    Backup,
    Temp,
    Orphan,
    OrphanDanglingPath,
    OrphanLegacyUnknown,
    OrphanExplicitlyUnregistered,
    InactiveSnapshot,
    Observed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFileInfo {
    pub path: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageHealthTotals {
    pub active_legacy_bytes: u64,
    pub active_legacy_files: u64,
    pub managed_derived_bytes: u64,
    pub managed_derived_files: u64,
    pub backup_bytes: u64,
    pub backup_files: u64,
    pub temp_bytes: u64,
    pub temp_files: u64,
    pub orphan_bytes: u64,
    pub orphan_files: u64,
    pub observed_bytes: u64,
    pub observed_files: u64,
    pub inactive_snapshot_bytes: u64,
    pub inactive_snapshot_files: u64,
    pub total_bytes: u64,
    pub total_files: u64,
}

impl StorageHealthTotals {
    fn accumulate(&mut self, kind: FileKind, bytes: u64) {
        self.total_bytes += bytes;
        self.total_files += 1;
        match kind {
            FileKind::ActiveLegacy => {
                self.active_legacy_bytes += bytes;
                self.active_legacy_files += 1;
            }
            FileKind::ManagedDerived => {
                self.managed_derived_bytes += bytes;
                self.managed_derived_files += 1;
            }
            FileKind::Backup => {
                self.backup_bytes += bytes;
                self.backup_files += 1;
            }
            FileKind::Temp => {
                self.temp_bytes += bytes;
                self.temp_files += 1;
            }
            FileKind::Orphan => {
                self.orphan_bytes += bytes;
                self.orphan_files += 1;
            }
            FileKind::OrphanDanglingPath
            | FileKind::OrphanLegacyUnknown
            | FileKind::OrphanExplicitlyUnregistered => {
                self.orphan_bytes += bytes;
                self.orphan_files += 1;
            }
            FileKind::Observed => {
                self.observed_bytes += bytes;
                self.observed_files += 1;
            }
            FileKind::InactiveSnapshot => {
                self.inactive_snapshot_bytes += bytes;
                self.inactive_snapshot_files += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealthReport {
    pub totals: StorageHealthTotals,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_offenders: Vec<StorageFileInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<StorageFileInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_status: Option<ManifestStatus>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<ObservedProjectUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_policy_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedProjectUsage {
    pub project_id: String,
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestStatus {
    pub index_exists: bool,
    pub workspace_count: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    pub active_materialized_bytes: u64,
    pub active_materialized_files: u64,
    pub inactive_materialized_bytes: u64,
    pub inactive_materialized_files: u64,
}

pub fn scan_storage_health(
    edges_dir: &Path,
    registered_project_ids: &HashSet<String>,
    project_filter: Option<&str>,
    include_files: bool,
) -> Result<StorageHealthReport> {
    let mut totals = StorageHealthTotals::default();
    let mut files: Vec<StorageFileInfo> = Vec::new();
    let project_facts = collect_project_storage_facts(edges_dir);

    scan_legacy_dir(
        edges_dir,
        registered_project_ids,
        &project_facts,
        project_filter,
        &mut totals,
        &mut files,
    )?;

    let managed_dir = edges_dir.join("derived");
    if managed_dir.is_dir() {
        scan_managed_derived_dir(
            &managed_dir,
            registered_project_ids,
            &project_facts,
            project_filter,
            &mut totals,
            &mut files,
        )?;
    }

    scan_inactive_snapshots(edges_dir, project_filter, &mut totals, &mut files);
    let observed = scan_observed_dir(edges_dir, project_filter, &mut totals, &mut files);

    files.sort_by_key(|b| std::cmp::Reverse(b.bytes));

    let top_offenders: Vec<StorageFileInfo> = files.iter().take(10).cloned().collect();

    let files_out = if include_files { files } else { Vec::new() };

    let manifest_status = scan_manifest_status(edges_dir);

    Ok(StorageHealthReport {
        totals,
        top_offenders,
        files: files_out,
        manifest_status,
        observed_policy_warning: observed_policy_warning(&observed),
        observed,
    })
}

fn observed_policy_warning(observed: &[ObservedProjectUsage]) -> Option<String> {
    let total = observed.iter().map(|o| o.bytes).sum::<u64>();
    if total == 0 {
        None
    } else {
        Some(format!(
            "observed_retention_keep_no_cap(total_bytes={total}); no observed pruning policy is configured"
        ))
    }
}

fn scan_manifest_status(edges_dir: &Path) -> Option<ManifestStatus> {
    let mat_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir);
    let mut inactive_bytes: u64 = 0;
    let mut inactive_files: u64 = 0;
    if mat_dir.is_dir() {
        let protected_jsonl_prefixes = collect_protected_jsonl_prefixes(edges_dir);
        if let Ok(entries) = fs::read_dir(&mat_dir) {
            for entry in entries.filter_map(Result::ok) {
                count_materialized_tree(
                    &entry.path(),
                    &protected_jsonl_prefixes,
                    &mut inactive_bytes,
                    &mut inactive_files,
                );
            }
        }
    }

    match bbox_edge_sidecar::manifest::try_load_manifest_index(edges_dir) {
        Ok(idx) => {
            let workspace_count = idx.workspaces.len();
            let active_paths = idx.active_materialized_paths(edges_dir);
            let (active_bytes, active_files) = count_paths_bytes(&active_paths);
            Some(ManifestStatus {
                index_exists: true,
                workspace_count,
                status: "valid".to_string(),
                fallback_reason: None,
                active_materialized_bytes: active_bytes,
                active_materialized_files: active_files,
                inactive_materialized_bytes: inactive_bytes,
                inactive_materialized_files: inactive_files,
            })
        }
        Err(reason) => match reason {
            bbox_edge_sidecar::manifest::ManifestFallbackReason::MissingNotMigrated => None,
            bbox_edge_sidecar::manifest::ManifestFallbackReason::Corrupt { error } => {
                Some(ManifestStatus {
                    index_exists: true,
                    workspace_count: 0,
                    status: "corrupt".to_string(),
                    fallback_reason: Some(error),
                    active_materialized_bytes: 0,
                    active_materialized_files: 0,
                    inactive_materialized_bytes: inactive_bytes,
                    inactive_materialized_files: inactive_files,
                })
            }
            bbox_edge_sidecar::manifest::ManifestFallbackReason::Stale {
                missing_manifests,
                missing_paths,
            } => Some(ManifestStatus {
                index_exists: true,
                workspace_count: 0,
                status: "stale".to_string(),
                fallback_reason: Some(format!(
                    "missing manifests: {:?}, missing paths: {:?}",
                    missing_manifests, missing_paths
                )),
                active_materialized_bytes: 0,
                active_materialized_files: 0,
                inactive_materialized_bytes: inactive_bytes,
                inactive_materialized_files: inactive_files,
            }),
        },
    }
}

fn collect_protected_jsonl_prefixes(edges_dir: &Path) -> Vec<String> {
    match bbox_edge_sidecar::manifest::try_load_manifest_index(edges_dir) {
        Ok(idx) => idx
            .protected_materialized_paths(edges_dir)
            .into_iter()
            .filter_map(|p| p.to_str().map(String::from))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn count_paths_bytes(paths: &[PathBuf]) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    for p in paths {
        if let Ok(meta) = fs::metadata(p) {
            bytes += meta.len();
            files += 1;
        }
    }
    (bytes, files)
}

fn count_materialized_tree(
    dir: &Path,
    active_prefixes: &[String],
    inactive_bytes: &mut u64,
    inactive_files: &mut u64,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            count_materialized_tree(&path, active_prefixes, inactive_bytes, inactive_files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let path_str = path.to_str().unwrap_or("");
            if !active_prefixes.iter().any(|prefix| path_str == prefix) {
                if let Ok(meta) = fs::metadata(&path) {
                    *inactive_bytes += meta.len();
                    *inactive_files += 1;
                }
            }
        }
    }
}

fn project_filter_matches(project_id: Option<&str>, project_filter: Option<&str>) -> bool {
    match (project_id, project_filter) {
        (_, None) => true,
        (Some(pid), Some(pf)) => pid == pf,
        (None, Some(_)) => false,
    }
}

fn scan_legacy_dir(
    edges_dir: &Path,
    registered_project_ids: &HashSet<String>,
    project_facts: &ProjectStorageFacts,
    project_filter: Option<&str>,
    totals: &mut StorageHealthTotals,
    files: &mut Vec<StorageFileInfo>,
) -> Result<()> {
    let entries = match fs::read_dir(edges_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if path.is_dir() {
            continue;
        }

        let bytes = match fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };

        if is_backup_file(file_name) {
            let project_id = extract_project_id_from_backup(file_name);
            if !project_filter_matches(project_id.as_deref(), project_filter) {
                continue;
            }
            totals.accumulate(FileKind::Backup, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::Backup,
                project_id,
                bytes,
                reason: None,
            });
            continue;
        }

        if is_temp_file(file_name) {
            let project_id = extract_project_id_from_base(file_name);
            if !project_filter_matches(project_id.as_deref(), project_filter) {
                continue;
            }
            totals.accumulate(FileKind::Temp, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::Temp,
                project_id,
                bytes,
                reason: None,
            });
            continue;
        }

        let extension = path.extension().and_then(|e| e.to_str());
        if extension != Some("jsonl") {
            continue;
        }

        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem == "agents" {
            if !project_filter_matches(Some("agents"), project_filter) {
                continue;
            }
            totals.accumulate(FileKind::ActiveLegacy, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::ActiveLegacy,
                project_id: Some("agents".to_string()),
                bytes,
                reason: None,
            });
            continue;
        }

        let project_id = stem.to_string();

        if !project_filter_matches(Some(&project_id), project_filter) {
            continue;
        }

        if registered_project_ids.contains(stem) {
            totals.accumulate(FileKind::ActiveLegacy, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::ActiveLegacy,
                project_id: Some(project_id),
                bytes,
                reason: None,
            });
        } else {
            let kind = project_facts.orphan_kind_for(&project_id);
            totals.accumulate(kind, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind,
                project_id: Some(project_id),
                bytes,
                reason: Some(orphan_reason(kind).to_string()),
            });
        }
    }

    Ok(())
}

fn scan_managed_derived_dir(
    managed_dir: &Path,
    registered_project_ids: &HashSet<String>,
    project_facts: &ProjectStorageFacts,
    project_filter: Option<&str>,
    totals: &mut StorageHealthTotals,
    files: &mut Vec<StorageFileInfo>,
) -> Result<()> {
    let namespace_entries = match fs::read_dir(managed_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for namespace_entry in namespace_entries.filter_map(Result::ok) {
        let namespace_path = namespace_entry.path();
        if !namespace_path.is_dir() {
            continue;
        }
        let namespace = namespace_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let sidecar_entries = match fs::read_dir(&namespace_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for sidecar_entry in sidecar_entries.filter_map(Result::ok) {
            let path = sidecar_entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            if path.is_dir() {
                continue;
            }

            let bytes = match fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(_) => continue,
            };

            if is_backup_file(file_name) {
                let project_id = extract_project_id_from_backup(file_name);
                if !project_filter_matches(project_id.as_deref(), project_filter) {
                    continue;
                }
                totals.accumulate(FileKind::Backup, bytes);
                files.push(StorageFileInfo {
                    path: path.display().to_string(),
                    kind: FileKind::Backup,
                    project_id,
                    bytes,
                    reason: None,
                });
                continue;
            }

            if is_temp_file(file_name) {
                let project_id = extract_project_id_from_base(file_name);
                if !project_filter_matches(project_id.as_deref(), project_filter) {
                    continue;
                }
                totals.accumulate(FileKind::Temp, bytes);
                files.push(StorageFileInfo {
                    path: path.display().to_string(),
                    kind: FileKind::Temp,
                    project_id,
                    bytes,
                    reason: None,
                });
                continue;
            }

            let extension = path.extension().and_then(|e| e.to_str());
            if extension != Some("jsonl") {
                continue;
            }

            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let project_id = stem.to_string();

            if !project_filter_matches(Some(&project_id), project_filter) {
                continue;
            }

            let is_registered = registered_project_ids.contains(stem);
            if is_registered {
                totals.accumulate(FileKind::ManagedDerived, bytes);
                files.push(StorageFileInfo {
                    path: path.display().to_string(),
                    kind: FileKind::ManagedDerived,
                    project_id: Some(project_id),
                    bytes,
                    reason: None,
                });
            } else {
                let kind = project_facts.orphan_kind_for(&project_id);
                totals.accumulate(kind, bytes);
                files.push(StorageFileInfo {
                    path: path.display().to_string(),
                    kind,
                    project_id: Some(project_id),
                    bytes,
                    reason: Some(format!("{} (namespace={namespace})", orphan_reason(kind))),
                });
            }
        }
    }

    Ok(())
}

#[derive(Default)]
struct ProjectStorageFacts {
    manifest_projects: HashSet<String>,
    dangling_projects: HashSet<String>,
    repo_by_project: HashMap<String, String>,
}

impl ProjectStorageFacts {
    fn orphan_kind_for(&self, project_id: &str) -> FileKind {
        if self.dangling_projects.contains(project_id) {
            FileKind::OrphanDanglingPath
        } else if self.manifest_projects.contains(project_id) {
            FileKind::OrphanExplicitlyUnregistered
        } else {
            FileKind::OrphanLegacyUnknown
        }
    }
}

fn orphan_reason(kind: FileKind) -> &'static str {
    match kind {
        FileKind::OrphanDanglingPath => "dangling_path project storage",
        FileKind::OrphanExplicitlyUnregistered => "explicitly_unregistered project storage",
        FileKind::OrphanLegacyUnknown => "legacy_unknown project sidecar",
        FileKind::Orphan => "unclassified orphan project storage",
        _ => "not orphan storage",
    }
}

fn collect_project_storage_facts(edges_dir: &Path) -> ProjectStorageFacts {
    let workspace_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir).join("workspace");
    let mut facts = ProjectStorageFacts::default();
    let Ok(entries) = fs::read_dir(workspace_dir) else {
        return facts;
    };
    for entry in entries.filter_map(Result::ok) {
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let Ok(manifest) =
            bbox_edge_sidecar::manifest::WorkspaceManifest::read_from(&manifest_path)
        else {
            continue;
        };
        facts.manifest_projects.insert(manifest.project_id.clone());
        if let Some(repo_id) = manifest.repo_id {
            facts
                .repo_by_project
                .insert(manifest.project_id.clone(), repo_id);
        }
        if manifest
            .canonical_path
            .as_deref()
            .is_some_and(|p| !Path::new(p).exists())
        {
            facts.dangling_projects.insert(manifest.project_id);
        }
    }
    facts
}

fn scan_inactive_snapshots(
    edges_dir: &Path,
    project_filter: Option<&str>,
    totals: &mut StorageHealthTotals,
    files: &mut Vec<StorageFileInfo>,
) {
    let active_prefixes = collect_protected_jsonl_prefixes(edges_dir);
    let mat_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir);
    if !mat_dir.is_dir() {
        return;
    }
    fn walk_for_inactive(
        dir: &Path,
        active_prefixes: &[String],
        project_filter: Option<&str>,
        totals: &mut StorageHealthTotals,
        files: &mut Vec<StorageFileInfo>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                walk_for_inactive(&path, active_prefixes, project_filter, totals, files);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let path_str = match path.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                if active_prefixes.iter().any(|p| path_str == p) {
                    continue;
                }
                let bytes = match fs::metadata(&path) {
                    Ok(m) => m.len(),
                    Err(_) => continue,
                };
                let project_id = extract_project_from_workspace_path(&path);
                if !project_filter_matches(project_id.as_deref(), project_filter) {
                    continue;
                }
                totals.accumulate(FileKind::InactiveSnapshot, bytes);
                files.push(StorageFileInfo {
                    path: path_str.to_string(),
                    kind: FileKind::InactiveSnapshot,
                    project_id,
                    bytes,
                    reason: Some("inactive snapshot not in active manifest paths".into()),
                });
            }
        }
    }
    walk_for_inactive(&mat_dir, &active_prefixes, project_filter, totals, files);
}

fn scan_observed_dir(
    edges_dir: &Path,
    project_filter: Option<&str>,
    totals: &mut StorageHealthTotals,
    files: &mut Vec<StorageFileInfo>,
) -> Vec<ObservedProjectUsage> {
    let observed_dir = edges_dir.join("observed");
    let mut usage: HashMap<String, ObservedProjectUsage> = HashMap::new();
    scan_observed_lane_dir(&observed_dir, project_filter, totals, files, &mut usage);
    let mut out: Vec<ObservedProjectUsage> = usage.into_values().collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.project_id.cmp(&b.project_id)));
    out
}

fn scan_observed_lane_dir(
    dir: &Path,
    project_filter: Option<&str>,
    totals: &mut StorageHealthTotals,
    files: &mut Vec<StorageFileInfo>,
    usage: &mut HashMap<String, ObservedProjectUsage>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            scan_observed_lane_dir(&path, project_filter, totals, files, usage);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(project_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !project_filter_matches(Some(&project_id), project_filter) {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let bytes = meta.len();
        let path_str = path.display().to_string();
        totals.accumulate(FileKind::Observed, bytes);
        files.push(StorageFileInfo {
            path: path_str.clone(),
            kind: FileKind::Observed,
            project_id: Some(project_id.clone()),
            bytes,
            reason: Some("observed history lane; retained by keep/no-cap policy".into()),
        });
        usage
            .entry(project_id.clone())
            .and_modify(|u| u.bytes += bytes)
            .or_insert(ObservedProjectUsage {
                project_id,
                path: path_str,
                bytes,
            });
    }
}

fn extract_project_from_workspace_path(path: &Path) -> Option<String> {
    let mut components = path.components().rev();
    let _filename = components.next()?;
    let _snapshot_id = components.next()?;
    let snapshots = components.next()?;
    if snapshots.as_os_str() != "snapshots" {
        return None;
    }
    let project = components.next()?;
    let workspace = components.next()?;
    if workspace.as_os_str() == "workspace" {
        Some(project.as_os_str().to_str()?.to_string())
    } else {
        None
    }
}

fn is_backup_file(file_name: &str) -> bool {
    if let Some(idx) = file_name.find(".bak-") {
        let rest = &file_name[idx + 5..];
        !rest.is_empty()
    } else {
        false
    }
}

fn is_temp_file(file_name: &str) -> bool {
    file_name.contains(".compact-") || file_name.ends_with(".tmp")
}

fn extract_project_id_from_backup(file_name: &str) -> Option<String> {
    let idx = file_name.find(".bak-")?;
    let base = &file_name[..idx];
    Some(base.strip_suffix(".jsonl").unwrap_or(base).to_string())
}

/// Extract the project_id base from filenames like `proj1234.jsonl`,
/// `proj1234.jsonl.tmp`, or `proj1234.jsonl.compact-1715600000-12345.tmp`.
/// Strips everything from the first `.jsonl` onward, then returns the
/// remaining leading segment.
fn extract_project_id_from_base(file_name: &str) -> Option<String> {
    let idx = file_name.find(".jsonl")?;
    let base = &file_name[..idx];
    if base.is_empty() {
        return None;
    }
    Some(base.to_string())
}

pub fn find_edges_dir(store_dir: &Path, projects_path: Option<&Path>) -> PathBuf {
    let from_store = crate::edge_index::edges_dir_from_bro_store(store_dir);
    if from_store.is_dir() {
        return from_store;
    }
    if let Some(pp) = projects_path {
        let from_projects = crate::edge_index::edges_dir_from_projects_path(pp);
        if from_projects.is_dir() {
            return from_projects;
        }
    }
    from_store
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcCandidate {
    pub path: String,
    pub kind: FileKind,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub rule: String,
    pub deletable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResult {
    pub applied: bool,
    pub candidates: Vec<GcCandidate>,
    pub deletable_count: usize,
    pub deletable_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_errors: Option<Vec<String>>,
}

pub struct GcParams {
    // kept: public GC param surface for orchestrators; `dry_run` consumed by callers (currently only outer tools)
    #[allow(dead_code)]
    pub dry_run: bool,
    pub project_filter: Option<String>,
    pub prune_backups: bool,
    pub prune_orphans: bool,
    pub prune_temps: bool,
    pub prune_inactive_snapshots: bool,
    pub max_backup_age_days: Option<u64>,
    pub keep_newest_backup_per_source: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcPolicy {
    pub materialized_snapshots: SnapshotRetentionPolicy,
    pub backups: BackupRetentionPolicy,
    pub orphans: OrphanRetentionPolicy,
    pub observed: ObservedRetentionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRetentionPolicy {
    pub keep_active: bool,
    pub keep_recent_per_workspace: u64,
    pub keep_recent_per_repo: u64,
    pub branch_switch_grace_minutes: u64,
    pub max_age_days: Option<u64>,
    /// Hard cap on retained inactive snapshot directories per workspace.
    /// Bounds the age-based keep: a snapshot under `max_age_days` is only
    /// retained while the workspace's retained count stays under this cap
    /// (floors — recent/grace — always retain and consume the cap). Without
    /// a count/byte budget, age-only retention reaches ~100 GB steady state
    /// at multi-agent commit rates (gap-efd270dd).
    pub max_count_per_workspace: Option<u64>,
    /// Total byte budget for retained inactive snapshots per workspace,
    /// consumed newest-first. Bounds the age-based keep the same way as
    /// `max_count_per_workspace`; floors always retain even over budget.
    pub max_total_bytes_per_workspace: Option<u64>,
}

impl Default for SnapshotRetentionPolicy {
    fn default() -> Self {
        Self {
            keep_active: true,
            keep_recent_per_workspace: 3,
            keep_recent_per_repo: 10,
            branch_switch_grace_minutes: 60,
            max_age_days: Some(14),
            max_count_per_workspace: Some(DEFAULT_SNAPSHOT_MAX_COUNT_PER_WORKSPACE),
            max_total_bytes_per_workspace: Some(DEFAULT_SNAPSHOT_MAX_TOTAL_BYTES_PER_WORKSPACE),
        }
    }
}

/// Default count budget for retained inactive snapshots per workspace.
pub const DEFAULT_SNAPSHOT_MAX_COUNT_PER_WORKSPACE: u64 = 32;
/// Default byte budget for retained inactive snapshots per workspace (16 GiB).
pub const DEFAULT_SNAPSHOT_MAX_TOTAL_BYTES_PER_WORKSPACE: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRetentionPolicy {
    pub max_total_bytes: Option<u64>,
}

impl Default for BackupRetentionPolicy {
    fn default() -> Self {
        Self {
            max_total_bytes: Some(2 * 1024 * 1024 * 1024),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanRetentionPolicy {
    pub auto_prune_after_days: u64,
    pub prune_explicitly_unregistered: bool,
}

impl Default for OrphanRetentionPolicy {
    fn default() -> Self {
        Self {
            auto_prune_after_days: 30,
            prune_explicitly_unregistered: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObservedRetentionPolicy {
    pub max_bytes_per_project: Option<u64>,
}

const TEMP_GRACE_SECS: u64 = 24 * 3600;

#[cfg(test)]
pub fn plan_gc(
    edges_dir: &Path,
    registered_project_ids: &HashSet<String>,
    params: &GcParams,
) -> Result<Vec<GcCandidate>> {
    plan_gc_with_policy(
        edges_dir,
        registered_project_ids,
        params,
        &GcPolicy::default(),
    )
}

pub fn plan_gc_with_policy(
    edges_dir: &Path,
    registered_project_ids: &HashSet<String>,
    params: &GcParams,
    policy: &GcPolicy,
) -> Result<Vec<GcCandidate>> {
    let report = scan_storage_health(
        edges_dir,
        registered_project_ids,
        params.project_filter.as_deref(),
        true,
    )?;

    let mut candidates: Vec<GcCandidate> = Vec::new();

    if params.prune_temps {
        for f in &report.files {
            if f.kind != FileKind::Temp {
                continue;
            }
            let path = Path::new(&f.path);
            let age_secs = file_age_secs(path).unwrap_or(0);
            if age_secs < TEMP_GRACE_SECS {
                candidates.push(GcCandidate {
                    path: f.path.clone(),
                    kind: f.kind,
                    bytes: f.bytes,
                    project_id: f.project_id.clone(),
                    rule: format!(
                        "temp_within_grace(age={}s,need={}s)",
                        age_secs, TEMP_GRACE_SECS
                    ),
                    deletable: false,
                });
                continue;
            }
            candidates.push(GcCandidate {
                path: f.path.clone(),
                kind: f.kind,
                bytes: f.bytes,
                project_id: f.project_id.clone(),
                rule: "temp_past_grace".to_string(),
                deletable: true,
            });
        }
    }

    if params.prune_backups {
        plan_backup_gc(&report.files, params, &policy.backups, &mut candidates);
    }

    if params.prune_orphans {
        let before = candidates.len();
        plan_orphan_gc(&report.files, &policy.orphans, &mut candidates);
        if candidates.len() == before {
            candidates.push(GcCandidate {
                path: String::new(),
                kind: FileKind::OrphanLegacyUnknown,
                bytes: 0,
                project_id: None,
                rule: "orphan_none_found".to_string(),
                deletable: false,
            });
        }
    }

    if params.prune_inactive_snapshots {
        let project_facts = collect_project_storage_facts(edges_dir);
        plan_snapshot_gc(
            &report.files,
            &project_facts,
            &policy.materialized_snapshots,
            &mut candidates,
        );
    }

    plan_observed_gc(&report.observed, &policy.observed, &mut candidates);

    candidates.sort_by(|a, b| a.rule.cmp(&b.rule).then(a.path.cmp(&b.path)));
    Ok(candidates)
}

fn plan_backup_gc(
    files: &[StorageFileInfo],
    params: &GcParams,
    policy: &BackupRetentionPolicy,
    candidates: &mut Vec<GcCandidate>,
) {
    let mut backups_by_source: HashMap<String, Vec<&StorageFileInfo>> = HashMap::new();
    for f in files {
        if f.kind != FileKind::Backup {
            continue;
        }
        let source_key = f
            .project_id
            .clone()
            .unwrap_or_else(|| path_source_key(&f.path));
        backups_by_source.entry(source_key).or_default().push(f);
    }

    let mut retained_backup_bytes = 0u64;
    let mut prunable_backup_refs: Vec<&StorageFileInfo> = Vec::new();
    for (source, mut backups) in backups_by_source {
        backups.sort_by_key(|a| backup_recency_key(a));
        let keep = params.keep_newest_backup_per_source as usize;
        for (i, f) in backups.iter().enumerate() {
            let age_secs = file_age_secs(Path::new(&f.path)).unwrap_or(0);
            let past_max_age = params
                .max_backup_age_days
                .is_some_and(|max_days| age_secs > max_days * 86400);
            if i < keep {
                retained_backup_bytes += f.bytes;
                candidates.push(GcCandidate {
                    path: f.path.clone(),
                    kind: f.kind,
                    bytes: f.bytes,
                    project_id: f.project_id.clone(),
                    rule: format!("backup_retained(#{},source={})", i + 1, source),
                    deletable: false,
                });
            } else {
                prunable_backup_refs.push(f);
                let age_note = if past_max_age {
                    format!(
                        "+past_max_age({}d)",
                        params.max_backup_age_days.unwrap_or_default()
                    )
                } else {
                    String::new()
                };
                candidates.push(GcCandidate {
                    path: f.path.clone(),
                    kind: f.kind,
                    bytes: f.bytes,
                    project_id: f.project_id.clone(),
                    rule: format!(
                        "backup_prunable(#{},keep={},source={}{})",
                        i + 1,
                        keep,
                        source,
                        age_note
                    ),
                    deletable: true,
                });
            }
        }
    }

    if let Some(max_total) = policy.max_total_bytes {
        let total_backup_bytes =
            retained_backup_bytes + prunable_backup_refs.iter().map(|f| f.bytes).sum::<u64>();
        if total_backup_bytes > max_total && retained_backup_bytes > max_total {
            candidates.push(GcCandidate {
                path: String::new(),
                kind: FileKind::Backup,
                bytes: retained_backup_bytes - max_total,
                project_id: None,
                rule: format!(
                    "backup_total_cap_exceeded_by_retained_newest(max_total_bytes={max_total})"
                ),
                deletable: false,
            });
        }
    }
}

fn plan_orphan_gc(
    files: &[StorageFileInfo],
    policy: &OrphanRetentionPolicy,
    candidates: &mut Vec<GcCandidate>,
) {
    let grace_secs = policy.auto_prune_after_days * 86400;
    for f in files {
        let is_auto_class = matches!(
            f.kind,
            FileKind::OrphanDanglingPath | FileKind::OrphanLegacyUnknown
        );
        let is_explicit = f.kind == FileKind::OrphanExplicitlyUnregistered;
        if !is_auto_class && !is_explicit && f.kind != FileKind::Orphan {
            continue;
        }

        let age_secs = file_age_secs(Path::new(&f.path)).unwrap_or(0);
        let deletable = if is_explicit {
            policy.prune_explicitly_unregistered && age_secs >= grace_secs
        } else {
            age_secs >= grace_secs
        };
        let rule = if is_explicit && !policy.prune_explicitly_unregistered {
            "orphan_explicitly_unregistered_operator_decision".to_string()
        } else if age_secs < grace_secs {
            format!(
                "orphan_within_grace(class={:?},age={}s,need={}s)",
                f.kind, age_secs, grace_secs
            )
        } else {
            format!(
                "orphan_auto_prune(class={:?},after_days={})",
                f.kind, policy.auto_prune_after_days
            )
        };
        candidates.push(GcCandidate {
            path: f.path.clone(),
            kind: f.kind,
            bytes: f.bytes,
            project_id: f.project_id.clone(),
            rule,
            deletable,
        });
    }
}

#[derive(Debug)]
struct SnapshotFileRef<'a> {
    file: &'a StorageFileInfo,
    snapshot_dir: String,
    project_id: String,
    repo_id: String,
    age_secs: u64,
}

fn plan_snapshot_gc(
    files: &[StorageFileInfo],
    project_facts: &ProjectStorageFacts,
    policy: &SnapshotRetentionPolicy,
    candidates: &mut Vec<GcCandidate>,
) {
    let mut snapshots: Vec<SnapshotFileRef<'_>> = Vec::new();
    for file in files {
        if file.kind != FileKind::InactiveSnapshot {
            continue;
        }
        let Some((project_id, snapshot_dir)) = inactive_snapshot_key(&file.path) else {
            continue;
        };
        let repo_id = project_facts
            .repo_by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_else(|| project_id.clone());
        snapshots.push(SnapshotFileRef {
            file,
            snapshot_dir,
            project_id,
            repo_id,
            age_secs: file_age_secs(Path::new(&file.path)).unwrap_or(0),
        });
    }

    // Aggregate to snapshot directories: retention decisions are per
    // snapshot (a head-<sha> directory), not per file — and the count/byte
    // budgets only make sense at that grain. A dir's age is its newest
    // file's age; its weight is the byte sum of its files.
    struct DirAgg {
        project_id: String,
        age_secs: u64,
        bytes: u64,
    }
    let mut dirs: HashMap<String, DirAgg> = HashMap::new();
    for snapshot in &snapshots {
        let entry = dirs
            .entry(snapshot.snapshot_dir.clone())
            .or_insert_with(|| DirAgg {
                project_id: snapshot.project_id.clone(),
                age_secs: snapshot.age_secs,
                bytes: 0,
            });
        entry.age_secs = entry.age_secs.min(snapshot.age_secs);
        entry.bytes += snapshot.file.bytes;
    }

    let retain_by_workspace = retained_snapshot_dirs(
        snapshots
            .iter()
            .map(|s| (&s.snapshot_dir, &s.project_id, s.age_secs)),
        policy.keep_recent_per_workspace,
    );
    let retain_by_repo = retained_snapshot_dirs(
        snapshots
            .iter()
            .map(|s| (&s.snapshot_dir, &s.repo_id, s.age_secs)),
        policy.keep_recent_per_repo,
    );
    let grace_secs = policy.branch_switch_grace_minutes * 60;

    // Walk each workspace's snapshot dirs newest-first, consuming the
    // count/byte budgets. Floors (recent workspace/repo, grace) always
    // retain and consume budget; the age-based keep applies only while
    // budget remains — that bound is what keeps steady-state disk usage
    // finite at high commit rates (gap-efd270dd).
    let mut by_workspace: HashMap<&str, Vec<(&String, &DirAgg)>> = HashMap::new();
    for (dir, agg) in &dirs {
        by_workspace
            .entry(agg.project_id.as_str())
            .or_default()
            .push((dir, agg));
    }
    // TODO: policy.keep_active is currently a no-op pending active-snapshot detection
    let mut dir_fate: HashMap<String, (bool, String)> = HashMap::new();
    for (_workspace, mut entries) in by_workspace {
        entries.sort_by(|a, b| a.1.age_secs.cmp(&b.1.age_secs).then(a.0.cmp(b.0)));
        let mut count_used: u64 = 0;
        let mut bytes_used: u64 = 0;
        for (dir, agg) in entries {
            let floor_reason = None
                .or_else(|| {
                    retain_by_workspace
                        .contains(dir)
                        .then(|| "snapshot_retained_recent_workspace".to_string())
                })
                .or_else(|| {
                    retain_by_repo
                        .contains(dir)
                        .then(|| "snapshot_retained_recent_repo".to_string())
                })
                .or_else(|| {
                    (agg.age_secs < grace_secs).then(|| {
                        format!(
                            "snapshot_retained_branch_switch_grace(age={}s,need={}s)",
                            agg.age_secs, grace_secs
                        )
                    })
                });

            if let Some(rule) = floor_reason {
                count_used += 1;
                bytes_used = bytes_used.saturating_add(agg.bytes);
                dir_fate.insert(dir.clone(), (false, rule));
                continue;
            }

            let under_age = policy
                .max_age_days
                .is_some_and(|max_days| agg.age_secs < max_days * 86400);
            if !under_age {
                dir_fate.insert(
                    dir.clone(),
                    (
                        true,
                        format!(
                            "snapshot_prunable(max_age_days={:?},keep_recent_per_workspace={},keep_recent_per_repo={})",
                            policy.max_age_days,
                            policy.keep_recent_per_workspace,
                            policy.keep_recent_per_repo
                        ),
                    ),
                );
                continue;
            }

            let over_count = policy
                .max_count_per_workspace
                .is_some_and(|cap| count_used >= cap);
            let over_bytes = policy
                .max_total_bytes_per_workspace
                .is_some_and(|cap| bytes_used.saturating_add(agg.bytes) > cap);
            if over_count || over_bytes {
                dir_fate.insert(
                    dir.clone(),
                    (
                        true,
                        format!(
                            "snapshot_prunable_over_budget(count_used={},max_count={:?},bytes_used={},dir_bytes={},max_bytes={:?})",
                            count_used,
                            policy.max_count_per_workspace,
                            bytes_used,
                            agg.bytes,
                            policy.max_total_bytes_per_workspace
                        ),
                    ),
                );
                continue;
            }

            count_used += 1;
            bytes_used = bytes_used.saturating_add(agg.bytes);
            dir_fate.insert(
                dir.clone(),
                (
                    false,
                    format!(
                        "snapshot_retained_under_max_age(age={}s,max_days={})",
                        agg.age_secs,
                        policy.max_age_days.unwrap_or(0)
                    ),
                ),
            );
        }
    }

    for snapshot in snapshots {
        let Some((deletable, rule)) = dir_fate.get(&snapshot.snapshot_dir) else {
            continue;
        };
        candidates.push(GcCandidate {
            path: snapshot.file.path.clone(),
            kind: snapshot.file.kind,
            bytes: snapshot.file.bytes,
            project_id: snapshot.file.project_id.clone(),
            rule: rule.clone(),
            deletable: *deletable,
        });
    }
}

fn retained_snapshot_dirs<'a>(
    items: impl Iterator<Item = (&'a String, &'a String, u64)>,
    keep: u64,
) -> HashSet<String> {
    if keep == 0 {
        return HashSet::new();
    }
    let mut by_bucket: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for (snapshot_dir, bucket, age_secs) in items {
        by_bucket
            .entry(bucket.clone())
            .or_default()
            .push((snapshot_dir.clone(), age_secs));
    }
    let mut retained = HashSet::new();
    for (_bucket, mut dirs) in by_bucket {
        dirs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        for (snapshot_dir, _) in dirs.into_iter().take(keep as usize) {
            retained.insert(snapshot_dir);
        }
    }
    retained
}

fn inactive_snapshot_key(path: &str) -> Option<(String, String)> {
    let path = Path::new(path);
    let mut components = path.components().rev();
    let _filename = components.next()?;
    let snapshot_id = components.next()?;
    let snapshots = components.next()?;
    if snapshots.as_os_str() != "snapshots" {
        return None;
    }
    let project = components.next()?;
    let workspace = components.next()?;
    if workspace.as_os_str() != "workspace" {
        return None;
    }
    let project_id = project.as_os_str().to_str()?.to_string();
    let snapshot_dir = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| snapshot_id.as_os_str().to_string_lossy().into_owned());
    Some((project_id, snapshot_dir))
}

fn plan_observed_gc(
    observed: &[ObservedProjectUsage],
    policy: &ObservedRetentionPolicy,
    candidates: &mut Vec<GcCandidate>,
) {
    for usage in observed {
        match policy.max_bytes_per_project {
            Some(max) if usage.bytes > max => candidates.push(GcCandidate {
                path: usage.path.clone(),
                kind: FileKind::Observed,
                bytes: usage.bytes - max,
                project_id: Some(usage.project_id.clone()),
                rule: format!("observed_over_cap_operator_review(max_bytes_per_project={max})"),
                deletable: false,
            }),
            None if usage.bytes > 0 => candidates.push(GcCandidate {
                path: usage.path.clone(),
                kind: FileKind::Observed,
                bytes: usage.bytes,
                project_id: Some(usage.project_id.clone()),
                rule: "observed_keep_no_cap".to_string(),
                deletable: false,
            }),
            _ => {}
        }
    }
}

pub fn apply_gc(candidates: &[GcCandidate]) -> (Vec<String>, Vec<String>) {
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    let mut snapshot_dirs: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    for c in candidates {
        if !c.deletable || c.path.is_empty() {
            continue;
        }
        match fs::remove_file(&c.path) {
            Ok(()) => {
                if c.kind == FileKind::InactiveSnapshot {
                    if let Some(parent) = Path::new(&c.path).parent() {
                        snapshot_dirs.insert(parent.to_path_buf());
                    }
                }
                deleted.push(c.path.clone());
            }
            Err(e) => errors.push(format!("{}: {}", c.path, e)),
        }
    }
    // Drop fully-pruned snapshot directories so dead `head-<sha>-…` dirs
    // don't accumulate (remove_dir refuses non-empty dirs, so a dir that
    // still holds retained files survives untouched).
    for dir in snapshot_dirs {
        let _ = fs::remove_dir(&dir);
    }
    (deleted, errors)
}

fn path_source_key(path: &str) -> String {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    extract_project_id_from_backup(file_name).unwrap_or_else(|| file_name.to_string())
}

fn backup_recency_key(f: &StorageFileInfo) -> (u64, Reverse<u64>) {
    let mtime = recency_age_secs(Path::new(&f.path));
    let suffix_ts = extract_bak_timestamp(&f.path).unwrap_or(0);
    (mtime, Reverse(suffix_ts))
}

fn extract_bak_timestamp(path: &str) -> Option<u64> {
    let file_name = Path::new(path).file_name().and_then(|n| n.to_str())?;
    let idx = file_name.find(".bak-")?;
    let ts_str = &file_name[idx + 5..];
    ts_str.parse().ok()
}

fn file_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let now = SystemTime::now();
    now.duration_since(modified).ok().map(|d| d.as_secs())
}

fn recency_age_secs(path: &Path) -> u64 {
    file_age_secs(path).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_edges_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_file(dir: &Path, name: &str, content: &[u8]) {
        fs::write(dir.join(name), content).unwrap();
    }

    fn set_mtime_days_old(path: &Path, days: i64) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_unix_time(1_700_000_000 - days * 86_400, 0),
        )
        .unwrap();
    }

    fn write_snapshot_jsonl(edges_dir: &Path, project_id: &str, snapshot_id: &str) -> PathBuf {
        let path = bbox_edge_sidecar::manifest::materialized_dir(edges_dir)
            .join("workspace")
            .join(project_id)
            .join("snapshots")
            .join(snapshot_id)
            .join("project.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"edge\":1}\n").unwrap();
        path
    }

    fn write_workspace_manifest(
        edges_dir: &Path,
        project_id: &str,
        repo_id: Option<&str>,
        canonical_path: Option<&Path>,
        active_snapshot_id: &str,
    ) {
        bbox_edge_sidecar::manifest::WorkspaceManifest::write_to(
            edges_dir,
            &bbox_edge_sidecar::manifest::WorkspaceManifest {
                version: 1,
                project_id: project_id.to_string(),
                repo_id: repo_id.map(str::to_string),
                canonical_path: canonical_path.map(|p| p.display().to_string()),
                git_common_dir: None,
                git_worktree_dir: None,
                branch: Some("main".into()),
                head_sha: Some(active_snapshot_id.into()),
                dirty: false,
                dirty_fingerprint: None,
                active_snapshot_id: Some(active_snapshot_id.into()),
                active_dirty_overlay_id: None,
                updated_at: None,
            },
        )
        .unwrap();
    }

    fn write_manifest_index(edges_dir: &Path, project_id: &str, active_snapshot_id: &str) {
        let mut idx = bbox_edge_sidecar::manifest::ManifestIndex::new();
        idx.upsert_workspace(
            project_id,
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!(
                    "workspace/{project_id}/snapshots/{active_snapshot_id}"
                )),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();
    }

    #[test]
    fn classifies_active_legacy_sidecar() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        let content = b"{\"edge\":1}\n";
        write_file(&edges_dir, "proj1234.jsonl", content);

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.active_legacy_files, 1);
        assert_eq!(report.totals.active_legacy_bytes, content.len() as u64);
        assert_eq!(report.totals.total_files, 1);
        assert!(report.files[0].path.contains("proj1234.jsonl"));
        assert_eq!(report.files[0].kind, FileKind::ActiveLegacy);
    }

    #[test]
    fn classifies_orphan_sidecar() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        let content = b"{\"edge\":1}\n";
        write_file(&edges_dir, "orphan12.jsonl", content);

        let registered = HashSet::new();
        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.orphan_files, 1);
        assert_eq!(report.totals.orphan_bytes, content.len() as u64);
        assert_eq!(report.files[0].kind, FileKind::OrphanLegacyUnknown);
        assert_eq!(
            report.files[0].reason.as_deref(),
            Some("legacy_unknown project sidecar")
        );
    }

    #[test]
    fn classifies_backup_file() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        let content = b"old data\n";
        write_file(&edges_dir, "proj1234.jsonl.bak-1715600000", content);

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.backup_files, 1);
        assert_eq!(report.totals.backup_bytes, content.len() as u64);
        assert_eq!(report.files[0].kind, FileKind::Backup);
        assert_eq!(report.files[0].project_id.as_deref(), Some("proj1234"));
    }

    #[test]
    fn classifies_temp_file() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        let content = b"temp\n";
        write_file(
            &edges_dir,
            "proj1234.jsonl.compact-1715600000-12345.tmp",
            content,
        );

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.temp_files, 1);
        assert_eq!(report.totals.temp_bytes, content.len() as u64);
        assert_eq!(report.files[0].kind, FileKind::Temp);
        assert_eq!(report.files[0].project_id.as_deref(), Some("proj1234"));
    }

    #[test]
    fn classifies_managed_derived_sidecar() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        let derived_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&derived_dir).unwrap();
        let content = b"{\"derived\":1}\n";
        write_file(&derived_dir, "proj1234.jsonl", content);

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.managed_derived_files, 1);
        assert_eq!(report.totals.managed_derived_bytes, content.len() as u64);
        assert_eq!(report.files[0].kind, FileKind::ManagedDerived);
    }

    #[test]
    fn classifies_orphan_managed_derived() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        let derived_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&derived_dir).unwrap();
        write_file(&derived_dir, "orphan12.jsonl", b"{\"derived\":1}\n");

        let registered = HashSet::new();
        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.orphan_files, 1);
        assert_eq!(report.files[0].kind, FileKind::OrphanLegacyUnknown);
        assert!(
            report.files[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("legacy_unknown")
        );
    }

    #[test]
    fn project_filter_isolates_single_project() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        let derived_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&derived_dir).unwrap();
        write_file(&edges_dir, "proj_aaaa.jsonl", b"a\n");
        write_file(&edges_dir, "proj_bbbb.jsonl", b"bb\n");
        write_file(&derived_dir, "proj_aaaa.jsonl", b"derived-a\n");

        let mut registered = HashSet::new();
        registered.insert("proj_aaaa".to_string());
        registered.insert("proj_bbbb".to_string());

        let report = scan_storage_health(&edges_dir, &registered, Some("proj_aaaa"), true).unwrap();
        assert_eq!(report.totals.active_legacy_files, 1);
        assert_eq!(report.totals.managed_derived_files, 1);
        assert_eq!(report.totals.total_files, 2);
        for f in &report.files {
            assert_eq!(f.project_id.as_deref(), Some("proj_aaaa"));
        }
    }

    #[test]
    fn project_filter_excludes_temps_for_other_projects() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "proj_aaaa.jsonl.compact-123-45.tmp", b"tmp-a\n");
        write_file(&edges_dir, "proj_bbbb.jsonl.compact-123-45.tmp", b"tmp-b\n");

        let mut registered = HashSet::new();
        registered.insert("proj_aaaa".to_string());
        registered.insert("proj_bbbb".to_string());

        let report = scan_storage_health(&edges_dir, &registered, Some("proj_aaaa"), true).unwrap();
        assert_eq!(report.totals.temp_files, 1);
        assert_eq!(report.totals.total_files, 1);
        assert_eq!(report.files[0].project_id.as_deref(), Some("proj_aaaa"));
    }

    #[test]
    fn agents_sidecar_classified_as_active_legacy() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "agents.jsonl", b"{\"agent\":1}\n");

        let registered = HashSet::new();
        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.active_legacy_files, 1);
        assert_eq!(report.files[0].project_id.as_deref(), Some("agents"));
    }

    #[test]
    fn top_offenders_capped_at_10() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let mut registered = HashSet::new();
        for i in 0..15 {
            let name = format!("proj{:04x}.jsonl", i);
            let content = vec![b'x'; (i + 1) * 100];
            write_file(&edges_dir, &name, &content);
            registered.insert(format!("proj{:04x}", i));
        }

        let report = scan_storage_health(&edges_dir, &registered, None, false).unwrap();
        assert_eq!(report.totals.total_files, 15);
        assert_eq!(report.top_offenders.len(), 10);
        assert!(report.files.is_empty());
        assert!(report.top_offenders[0].bytes >= report.top_offenders[1].bytes);
    }

    #[test]
    fn mixed_file_types_counted_correctly() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        let derived_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&derived_dir).unwrap();

        write_file(&edges_dir, "proj1234.jsonl", b"active\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-111", b"backup\n");
        write_file(&edges_dir, "proj1234.jsonl.compact-222-33.tmp", b"tmp\n");
        write_file(&edges_dir, "orphan99.jsonl", b"orphan\n");
        write_file(&derived_dir, "proj1234.jsonl", b"derived\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.active_legacy_files, 1);
        assert_eq!(report.totals.managed_derived_files, 1);
        assert_eq!(report.totals.backup_files, 1);
        assert_eq!(report.totals.temp_files, 1);
        assert_eq!(report.totals.orphan_files, 1);
        assert_eq!(report.totals.total_files, 5);
    }

    #[test]
    fn empty_edges_dir_returns_zero_totals() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let registered = HashSet::new();
        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.total_files, 0);
        assert_eq!(report.totals.total_bytes, 0);
        assert!(report.files.is_empty());
        assert!(report.top_offenders.is_empty());
    }

    #[test]
    fn non_existent_edges_dir_returns_zero() {
        let dir = setup_edges_dir();
        let edges_dir = dir.path().join("nonexistent");

        let registered = HashSet::new();
        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.total_files, 0);
    }

    #[test]
    fn backup_file_detection_patterns() {
        assert!(is_backup_file("proj1234.jsonl.bak-1715600000"));
        assert!(is_backup_file("abc.jsonl.bak-123"));
        assert!(is_backup_file("p1.jsonl.bak-migrated-1715600000"));
        assert!(!is_backup_file("proj1234.jsonl"));
        assert!(!is_backup_file("proj1234.jsonl.tmp"));
        assert!(!is_backup_file("proj1234.jsonl.compact-123-45.tmp"));
    }

    #[test]
    fn temp_file_detection_patterns() {
        assert!(is_temp_file("proj1234.jsonl.compact-1715600000-12345.tmp"));
        assert!(is_temp_file("abc.jsonl.tmp"));
        assert!(!is_temp_file("proj1234.jsonl"));
        assert!(!is_temp_file("proj1234.jsonl.bak-123"));
    }

    #[test]
    fn extract_project_id_variants() {
        assert_eq!(
            extract_project_id_from_backup("proj1234.jsonl.bak-1715600000"),
            Some("proj1234".to_string())
        );
        assert_eq!(
            extract_project_id_from_backup("abc.jsonl.bak-123"),
            Some("abc".to_string())
        );
        assert_eq!(
            extract_project_id_from_base("proj1234.jsonl.tmp"),
            Some("proj1234".to_string())
        );
        assert_eq!(
            extract_project_id_from_base("proj1234.jsonl.compact-1715600000-12345.tmp"),
            Some("proj1234".to_string())
        );
        assert_eq!(
            extract_project_id_from_base("proj1234.jsonl"),
            Some("proj1234".to_string())
        );
        assert_eq!(extract_project_id_from_base(".jsonl"), None);
    }

    #[test]
    fn gc_dry_run_reports_candidates_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "proj1234.jsonl.bak-1000", b"old backup\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-2000", b"newer backup\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        let deletable: Vec<&GcCandidate> = candidates.iter().filter(|c| c.deletable).collect();
        assert!(
            !deletable.is_empty(),
            "should have at least one deletable backup"
        );
        assert!(
            edges_dir.join("proj1234.jsonl.bak-1000").exists(),
            "dry_run must not delete files"
        );
        assert!(
            edges_dir.join("proj1234.jsonl.bak-2000").exists(),
            "dry_run must not delete files"
        );
    }

    #[test]
    fn gc_apply_deletes_only_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "proj1234.jsonl", b"active\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-1000", b"old backup\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-2000", b"newer backup\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: false,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        let (deleted, errors) = apply_gc(&candidates);
        assert!(errors.is_empty(), "no delete errors expected");
        assert!(
            edges_dir.join("proj1234.jsonl").exists(),
            "active sidecar must never be deleted"
        );
        assert!(
            !deleted
                .iter()
                .any(|p| p.contains("proj1234.jsonl") && !p.contains(".bak-")),
            "active sidecar must not appear in deleted list"
        );
    }

    #[test]
    fn gc_retains_newest_backup_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "proj1234.jsonl.bak-1000", b"old\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-2000", b"mid\n");
        write_file(&edges_dir, "proj1234.jsonl.bak-3000", b"newest\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        let retained: Vec<&GcCandidate> = candidates
            .iter()
            .filter(|c| !c.deletable && c.kind == FileKind::Backup)
            .collect();
        assert_eq!(retained.len(), 1, "exactly 1 newest backup retained");
        assert!(
            retained[0].rule.contains("retained"),
            "retained backup rule must say retained"
        );
        assert!(
            retained[0].path.contains("bak-3000"),
            "highest suffix (newest by timestamp) must be retained, got {:?}",
            retained[0].path
        );
    }

    #[test]
    fn gc_mtime_overrides_suffix_for_recency() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let newer_path = edges_dir.join("projAB.jsonl.bak-100");
        let older_path = edges_dir.join("projAB.jsonl.bak-9999");
        fs::write(&newer_path, b"x\n").unwrap();
        fs::write(&older_path, b"x\n").unwrap();

        filetime::set_file_mtime(
            &newer_path,
            filetime::FileTime::from_unix_time(1700000000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            &older_path,
            filetime::FileTime::from_unix_time(1600000000, 0),
        )
        .unwrap();

        let mut registered = HashSet::new();
        registered.insert("projAB".to_string());

        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        let retained: Vec<&GcCandidate> = candidates
            .iter()
            .filter(|c| !c.deletable && c.kind == FileKind::Backup)
            .collect();
        assert_eq!(retained.len(), 1);
        assert!(
            retained[0].path.contains("bak-100"),
            "newer mtime must win over higher suffix, got {:?}",
            retained[0].path
        );
    }

    #[test]
    fn gc_respects_temp_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        let recent_path = edges_dir.join("proj1234.jsonl.compact-123-45.tmp");
        fs::write(&recent_path, b"recent tmp\n").unwrap();

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: true,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        let grace_candidates: Vec<&GcCandidate> = candidates
            .iter()
            .filter(|c| c.rule.contains("grace"))
            .collect();
        assert!(
            !grace_candidates.is_empty(),
            "recent temp file must appear in candidates (within grace or prunable)"
        );
    }

    #[test]
    fn gc_orphans_reported_not_deleted_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "orphan99.jsonl", b"orphan data\n");

        let registered = HashSet::new();

        let candidates_default = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();
        assert!(
            candidates_default.is_empty(),
            "orphans not candidates when prune_orphans=false"
        );

        let candidates_report = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: true,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();
        assert!(
            candidates_report
                .iter()
                .any(|c| c.rule.contains("orphan_within_grace")),
            "recent legacy_unknown orphans must be reported but retained within grace"
        );

        let (deleted, _) = apply_gc(&candidates_report);
        assert!(deleted.is_empty(), "Phase 1 must not delete orphans");
        assert!(
            edges_dir.join("orphan99.jsonl").exists(),
            "orphan file must survive apply"
        );
    }

    #[test]
    fn observed_history_is_reported_per_project_and_cap_warns_without_delete() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        let observed_dir = edges_dir.join("observed");
        fs::create_dir_all(&observed_dir).unwrap();
        write_file(&observed_dir, "proj1234.jsonl", b"observed history\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let report = scan_storage_health(&edges_dir, &registered, None, true).unwrap();
        assert_eq!(report.totals.observed_files, 1);
        assert_eq!(report.observed.len(), 1);
        assert_eq!(report.observed[0].project_id, "proj1234");
        assert!(report.observed_policy_warning.is_some());

        let candidates = plan_gc_with_policy(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &GcPolicy {
                observed: ObservedRetentionPolicy {
                    max_bytes_per_project: Some(4),
                },
                ..GcPolicy::default()
            },
        )
        .unwrap();

        let observed = candidates
            .iter()
            .find(|c| c.kind == FileKind::Observed)
            .expect("observed over-cap candidate expected");
        assert!(observed.rule.contains("observed_over_cap"));
        assert!(!observed.deletable);
    }

    #[test]
    fn inactive_snapshot_policy_retains_recent_and_prunes_old() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let active = write_snapshot_jsonl(&edges_dir, "p1", "head-active");
        let recent = write_snapshot_jsonl(&edges_dir, "p1", "head-recent");
        let old = write_snapshot_jsonl(&edges_dir, "p1", "head-old");
        set_mtime_days_old(&active, 0);
        set_mtime_days_old(&recent, 1);
        set_mtime_days_old(&old, 10);
        write_workspace_manifest(
            &edges_dir,
            "p1",
            Some("repo1"),
            Some(dir.path()),
            "head-active",
        );
        write_manifest_index(&edges_dir, "p1", "head-active");

        let registered: HashSet<String> = ["p1".to_string()].into_iter().collect();
        let candidates = plan_gc_with_policy(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: true,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &GcPolicy {
                materialized_snapshots: SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: 1,
                    keep_recent_per_repo: 0,
                    branch_switch_grace_minutes: 0,
                    max_age_days: Some(0),
                    max_count_per_workspace: None,
                    max_total_bytes_per_workspace: None,
                },
                ..GcPolicy::default()
            },
        )
        .unwrap();

        assert!(
            !candidates.iter().any(|c| c.path.contains("head-active")),
            "active snapshot path must not become a candidate"
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.path.contains("head-recent") && !c.deletable)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.path.contains("head-old") && c.deletable)
        );
    }

    /// The count budget bounds the age-based keep (gap-efd270dd): with
    /// max_count_per_workspace=2, the recent floor plus one under-age
    /// snapshot retain; everything older prunes even though far under
    /// max_age_days.
    #[test]
    fn inactive_snapshot_count_budget_bounds_age_keep() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let active = write_snapshot_jsonl(&edges_dir, "p1", "head-active");
        set_mtime_days_old(&active, 0);
        for (id, days) in [("head-b", 1), ("head-c", 2), ("head-d", 3), ("head-e", 4)] {
            let path = write_snapshot_jsonl(&edges_dir, "p1", id);
            set_mtime_days_old(&path, days);
        }
        write_workspace_manifest(
            &edges_dir,
            "p1",
            Some("repo1"),
            Some(dir.path()),
            "head-active",
        );
        write_manifest_index(&edges_dir, "p1", "head-active");

        let registered: HashSet<String> = ["p1".to_string()].into_iter().collect();
        let candidates = plan_gc_with_policy(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: true,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &GcPolicy {
                materialized_snapshots: SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: 1,
                    keep_recent_per_repo: 0,
                    branch_switch_grace_minutes: 0,
                    // Everything is under age — only the budget can prune.
                    max_age_days: Some(10_000),
                    max_count_per_workspace: Some(2),
                    max_total_bytes_per_workspace: None,
                },
                ..GcPolicy::default()
            },
        )
        .unwrap();

        let fate = |needle: &str| {
            candidates
                .iter()
                .find(|c| c.path.contains(needle))
                .map(|c| (c.deletable, c.rule.clone()))
        };
        assert_eq!(
            fate("head-b").map(|f| f.0),
            Some(false),
            "newest inactive snapshot is floor-retained"
        );
        assert_eq!(
            fate("head-c").map(|f| f.0),
            Some(false),
            "second snapshot fits the count budget"
        );
        for id in ["head-d", "head-e"] {
            let (deletable, rule) = fate(id).expect("candidate exists");
            assert!(deletable, "{id} must prune over the count budget: {rule}");
            assert!(
                rule.starts_with("snapshot_prunable_over_budget"),
                "{id} rule must name the budget: {rule}"
            );
        }
    }

    /// The byte budget prunes under-age snapshots once the workspace's
    /// retained bytes exceed the ceiling — but floors always win over the
    /// budget (never delete the recent floor to satisfy bytes).
    #[test]
    fn inactive_snapshot_byte_budget_bounds_age_keep_but_floors_win() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let active = write_snapshot_jsonl(&edges_dir, "p1", "head-active");
        set_mtime_days_old(&active, 0);
        let recent = write_snapshot_jsonl(&edges_dir, "p1", "head-recent");
        set_mtime_days_old(&recent, 1);
        let older = write_snapshot_jsonl(&edges_dir, "p1", "head-older");
        set_mtime_days_old(&older, 2);
        write_workspace_manifest(
            &edges_dir,
            "p1",
            Some("repo1"),
            Some(dir.path()),
            "head-active",
        );
        write_manifest_index(&edges_dir, "p1", "head-active");

        let registered: HashSet<String> = ["p1".to_string()].into_iter().collect();
        let candidates = plan_gc_with_policy(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: true,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &GcPolicy {
                materialized_snapshots: SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: 1,
                    keep_recent_per_repo: 0,
                    branch_switch_grace_minutes: 0,
                    max_age_days: Some(10_000),
                    max_count_per_workspace: None,
                    // Smaller than a single snapshot file: the floor still
                    // retains; everything else is over budget.
                    max_total_bytes_per_workspace: Some(1),
                },
                ..GcPolicy::default()
            },
        )
        .unwrap();

        let recent_candidate = candidates
            .iter()
            .find(|c| c.path.contains("head-recent"))
            .expect("recent snapshot is a candidate");
        assert!(
            !recent_candidate.deletable,
            "floor-retained snapshot survives even over the byte budget: {}",
            recent_candidate.rule
        );
        let older_candidate = candidates
            .iter()
            .find(|c| c.path.contains("head-older"))
            .expect("older snapshot is a candidate");
        assert!(
            older_candidate.deletable,
            "under-age snapshot over the byte budget must prune: {}",
            older_candidate.rule
        );
        assert!(
            older_candidate
                .rule
                .starts_with("snapshot_prunable_over_budget"),
            "rule must name the budget: {}",
            older_candidate.rule
        );
    }

    #[test]
    fn orphan_classes_have_separate_delete_gates() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "legacy_unknown.jsonl", b"legacy\n");
        let legacy_path = edges_dir.join("legacy_unknown.jsonl");
        set_mtime_days_old(&legacy_path, 40);

        let missing_path = dir.path().join("missing-project");
        write_workspace_manifest(
            &edges_dir,
            "dangling",
            Some("repo1"),
            Some(&missing_path),
            "head-active",
        );
        write_file(&edges_dir, "dangling.jsonl", b"dangling\n");
        let dangling_path = edges_dir.join("dangling.jsonl");
        set_mtime_days_old(&dangling_path, 40);

        let existing_path = dir.path().join("existing-project");
        fs::create_dir_all(&existing_path).unwrap();
        write_workspace_manifest(
            &edges_dir,
            "explicit",
            Some("repo1"),
            Some(&existing_path),
            "head-active",
        );
        write_file(&edges_dir, "explicit.jsonl", b"explicit\n");
        let explicit_path = edges_dir.join("explicit.jsonl");
        set_mtime_days_old(&explicit_path, 40);

        let registered = HashSet::new();
        let candidates = plan_gc(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: false,
                prune_orphans: true,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
        )
        .unwrap();

        assert!(
            candidates
                .iter()
                .any(|c| { c.kind == FileKind::OrphanLegacyUnknown && c.deletable })
        );
        assert!(
            candidates
                .iter()
                .any(|c| { c.kind == FileKind::OrphanDanglingPath && c.deletable })
        );
        assert!(
            candidates
                .iter()
                .any(|c| { c.kind == FileKind::OrphanExplicitlyUnregistered && !c.deletable })
        );
    }

    #[test]
    fn backup_total_cap_reports_retained_newest_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();
        write_file(&edges_dir, "proj1234.jsonl.bak-1000", b"large backup\n");

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());

        let candidates = plan_gc_with_policy(
            &edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: false,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &GcPolicy {
                backups: BackupRetentionPolicy {
                    max_total_bytes: Some(1),
                },
                ..GcPolicy::default()
            },
        )
        .unwrap();

        assert!(candidates.iter().any(|c| {
            c.kind == FileKind::Backup
                && c.rule.contains("backup_total_cap_exceeded")
                && !c.deletable
        }));
    }
}
