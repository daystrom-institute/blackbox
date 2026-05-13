use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub total_bytes: u64,
    pub total_files: u64,
}

impl Default for StorageHealthTotals {
    fn default() -> Self {
        Self {
            active_legacy_bytes: 0,
            active_legacy_files: 0,
            managed_derived_bytes: 0,
            managed_derived_files: 0,
            backup_bytes: 0,
            backup_files: 0,
            temp_bytes: 0,
            temp_files: 0,
            orphan_bytes: 0,
            orphan_files: 0,
            total_bytes: 0,
            total_files: 0,
        }
    }
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
}

pub fn scan_storage_health(
    edges_dir: &Path,
    registered_project_ids: &HashSet<String>,
    project_filter: Option<&str>,
    include_files: bool,
) -> Result<StorageHealthReport> {
    let mut totals = StorageHealthTotals::default();
    let mut files: Vec<StorageFileInfo> = Vec::new();

    scan_legacy_dir(
        edges_dir,
        registered_project_ids,
        project_filter,
        &mut totals,
        &mut files,
    )?;

    let managed_dir = edges_dir.join("derived");
    if managed_dir.is_dir() {
        scan_managed_derived_dir(
            &managed_dir,
            registered_project_ids,
            project_filter,
            &mut totals,
            &mut files,
        )?;
    }

    files.sort_by(|a, b| b.bytes.cmp(&a.bytes));

    let top_offenders: Vec<StorageFileInfo> = files.iter().take(10).cloned().collect();

    let files_out = if include_files { files } else { Vec::new() };

    Ok(StorageHealthReport {
        totals,
        top_offenders,
        files: files_out,
    })
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

        let is_registered = registered_project_ids.contains(stem);
        let project_id = stem.to_string();

        if !project_filter_matches(Some(&project_id), project_filter) {
            continue;
        }

        if is_registered {
            totals.accumulate(FileKind::ActiveLegacy, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::ActiveLegacy,
                project_id: Some(project_id),
                bytes,
                reason: None,
            });
        } else {
            totals.accumulate(FileKind::Orphan, bytes);
            files.push(StorageFileInfo {
                path: path.display().to_string(),
                kind: FileKind::Orphan,
                project_id: Some(project_id),
                bytes,
                reason: Some("unregistered project sidecar".to_string()),
            });
        }
    }

    Ok(())
}

fn scan_managed_derived_dir(
    managed_dir: &Path,
    registered_project_ids: &HashSet<String>,
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
                totals.accumulate(FileKind::Orphan, bytes);
                files.push(StorageFileInfo {
                    path: path.display().to_string(),
                    kind: FileKind::Orphan,
                    project_id: Some(project_id),
                    bytes,
                    reason: Some(format!(
                        "unregistered managed derived sidecar (namespace={namespace})"
                    )),
                });
            }
        }
    }

    Ok(())
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

pub(crate) fn find_edges_dir(store_dir: &Path, projects_path: Option<&Path>) -> PathBuf {
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
        assert_eq!(report.files[0].kind, FileKind::Orphan);
        assert_eq!(
            report.files[0].reason.as_deref(),
            Some("unregistered project sidecar")
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
        assert_eq!(report.files[0].kind, FileKind::Orphan);
        assert!(
            report.files[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("unregistered managed derived")
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
}
