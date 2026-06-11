use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gaps::{GapNote, GapStore};

const GAP_NOTE_TYPE: &str = "blackbox.gap_note.v1";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GapSpoolImportReport {
    pub imported: Vec<ImportedGapFile>,
    pub rejected: Vec<RejectedGapFile>,
    pub skipped: Vec<SkippedGapFile>,
}

impl GapSpoolImportReport {
    pub fn is_empty(&self) -> bool {
        self.imported.is_empty() && self.rejected.is_empty() && self.skipped.is_empty()
    }

    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let total = self.imported.len() + self.rejected.len() + self.skipped.len();
        let mut out = format!("## Imported gap notes ({total})\n");
        for item in &self.imported {
            let project = item.project.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "  imported {} -> {} [{}]\n",
                item.path, item.gap_id, project
            ));
        }
        for item in &self.skipped {
            out.push_str(&format!("  skipped {} — {}\n", item.path, item.reason));
        }
        for item in &self.rejected {
            out.push_str(&format!("  rejected {} — {}\n", item.path, item.error));
        }
        out.push('\n');
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedGapFile {
    pub path: String,
    pub moved_to: String,
    pub gap_id: String,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedGapFile {
    pub path: String,
    pub moved_to: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedGapFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct SpoolSource {
    inbox_dir: PathBuf,
    project: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ImportState {
    version: u32,
    imported: Vec<ImportedFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportedFingerprint {
    path: String,
    sha256: String,
    gap_id: String,
    imported_at: String,
}

/// `project_roots` are canonical project paths (the caller maps them from
/// its project registry); each contributes a `.bbox/gaps/inbox` spool dir.
pub fn import_gap_spool(
    gaps: &mut GapStore,
    project_roots: &[String],
    state_dir: &Path,
) -> Result<GapSpoolImportReport> {
    let host_inbox = host_gap_inbox_dir();
    import_gap_spool_from_dirs(gaps, project_roots, state_dir, Some(host_inbox))
}

pub fn import_gap_spool_from_dirs(
    gaps: &mut GapStore,
    project_roots: &[String],
    state_dir: &Path,
    host_inbox: Option<PathBuf>,
) -> Result<GapSpoolImportReport> {
    let state_path = state_dir.join("gap-spool-imports.json");
    let mut state = load_state(&state_path)?;
    let mut report = GapSpoolImportReport::default();

    for source in spool_sources(project_roots, host_inbox) {
        let entries = match fs::read_dir(&source.inbox_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                report.rejected.push(RejectedGapFile {
                    path: source.inbox_dir.display().to_string(),
                    moved_to: None,
                    error: err.to_string(),
                });
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            import_one(gaps, &mut state, &mut report, &source, &path)?;
        }
    }

    save_state(&state_path, &state)?;
    Ok(report)
}

fn import_one(
    gaps: &mut GapStore,
    state: &mut ImportState,
    report: &mut GapSpoolImportReport,
    source: &SpoolSource,
    path: &Path,
) -> Result<()> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let hash = sha256_hex(&raw);
    let path_s = path.display().to_string();
    if state
        .imported
        .iter()
        .any(|fp| fp.path == path_s && fp.sha256 == hash)
    {
        report.skipped.push(SkippedGapFile {
            path: path_s,
            reason: "already imported fingerprint".into(),
        });
        return Ok(());
    }

    let raw_text = match String::from_utf8(raw) {
        Ok(text) => text,
        Err(err) => {
            reject_file(report, source, path, format!("not utf-8: {err}"))?;
            return Ok(());
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&raw_text) {
        Ok(value) => value,
        Err(err) => {
            reject_file(report, source, path, format!("invalid JSON: {err}"))?;
            return Ok(());
        }
    };
    let Some(object) = value.as_object() else {
        reject_file(
            report,
            source,
            path,
            "gap file must be a JSON object".into(),
        )?;
        return Ok(());
    };
    if object.get("type").and_then(|v| v.as_str()) != Some(GAP_NOTE_TYPE) {
        reject_file(
            report,
            source,
            path,
            format!("missing type={GAP_NOTE_TYPE}"),
        )?;
        return Ok(());
    }

    // Build the typed gap from the envelope; per-file rejection on a malformed
    // or incomplete envelope (missing required field). Open-duplicate dedupe by
    // `dedupe_key` + scope is handled inside `GapStore::ingest`.
    let mut gap = match GapNote::from_envelope(&value, String::new(), crate::util::now_iso()) {
        Ok(gap) => gap,
        Err(err) => {
            reject_file(report, source, path, format!("invalid gap envelope: {err}"))?;
            return Ok(());
        }
    };
    gap.project = source.project.clone();

    let (gap_id, created) = gaps.ingest(gap)?;
    let moved_to = move_file(path, &source.inbox_dir.join("imported"))?;
    if created {
        state.imported.push(ImportedFingerprint {
            path: path.display().to_string(),
            sha256: hash,
            gap_id: gap_id.clone(),
            imported_at: crate::util::now_iso(),
        });
        report.imported.push(ImportedGapFile {
            path: path.display().to_string(),
            moved_to: moved_to.display().to_string(),
            gap_id,
            project: source.project.clone(),
        });
    } else {
        report.skipped.push(SkippedGapFile {
            path: path.display().to_string(),
            reason: "live gap with same dedupe_key already open".into(),
        });
    }
    Ok(())
}

fn reject_file(
    report: &mut GapSpoolImportReport,
    source: &SpoolSource,
    path: &Path,
    error: String,
) -> Result<()> {
    let moved_to = move_file(path, &source.inbox_dir.join("rejected"))?;
    let error_path = moved_to.with_extension("json.error.txt");
    fs::write(&error_path, &error).with_context(|| format!("writing {}", error_path.display()))?;
    report.rejected.push(RejectedGapFile {
        path: path.display().to_string(),
        moved_to: Some(moved_to.display().to_string()),
        error,
    });
    Ok(())
}

fn move_file(path: &Path, target_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(target_dir).with_context(|| format!("creating {}", target_dir.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("path has no file name: {}", path.display()))?;
    let mut target = target_dir.join(file_name);
    if target.exists() {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("gap");
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("json");
        let suffix = sha256_hex(path.display().to_string().as_bytes());
        target = target_dir.join(format!("{stem}-{}.{}", &suffix[..8], ext));
    }
    fs::rename(path, &target)
        .with_context(|| format!("moving {} to {}", path.display(), target.display()))?;
    Ok(target)
}

fn spool_sources(project_roots: &[String], host_inbox: Option<PathBuf>) -> Vec<SpoolSource> {
    let mut out = Vec::new();
    for canonical_path in project_roots {
        let root = PathBuf::from(canonical_path);
        out.push(SpoolSource {
            inbox_dir: root.join(".bbox").join("gaps").join("inbox"),
            project: Some(canonical_path.clone()),
        });
    }
    if let Some(inbox_dir) = host_inbox {
        out.push(SpoolSource {
            inbox_dir,
            project: None,
        });
    }
    out
}

fn host_gap_inbox_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
    });
    base.join("blackbox").join("gaps").join("inbox")
}

fn load_state(path: &Path) -> Result<ImportState> {
    if !path.exists() {
        return Ok(ImportState {
            version: 1,
            imported: Vec::new(),
        });
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn save_state(path: &Path, state: &ImportState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::json_store::atomic_write_json_locked(path, state)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn gap_body(title: &str, dedupe_slug: &str) -> String {
        serde_json::json!({
            "type": GAP_NOTE_TYPE,
            "title": title,
            "gap_kind": "workflow",
            "domain": "gap-spool-test",
            "wanted_capability": title,
            "impact": "high",
            "dedupe_key": format!("workflow/gap-spool-test/{dedupe_slug}")
        })
        .to_string()
    }

    #[test]
    fn imports_project_local_gap_file_and_moves_it() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        let inbox = project.join(".bbox/gaps/inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            inbox.join("gap.json"),
            gap_body("Need workflow latch", "latch"),
        )
        .unwrap();

        let canonical = project.canonicalize().unwrap().to_string_lossy().to_string();
        let roots = vec![canonical.clone()];
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let report =
            import_gap_spool_from_dirs(&mut gaps, &roots, &dir.path().join("state"), None)
                .unwrap();

        assert_eq!(report.imported.len(), 1);
        assert!(inbox.join("imported/gap.json").exists());
        assert_eq!(gaps.all().len(), 1);
        assert_eq!(
            gaps.all()[0].project.as_deref(),
            Some(canonical.as_str())
        );
    }

    #[test]
    fn imports_host_wide_gap_file_without_project() {
        let dir = tempdir().unwrap();
        let host = dir.path().join("host/inbox");
        fs::create_dir_all(&host).unwrap();
        fs::write(
            host.join("gap.json"),
            gap_body("Need host hook", "host-hook"),
        )
        .unwrap();
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let report = import_gap_spool_from_dirs(
            &mut gaps,
            &[],
            &dir.path().join("state"),
            Some(host.clone()),
        )
        .unwrap();

        assert_eq!(report.imported.len(), 1);
        assert!(host.join("imported/gap.json").exists());
        assert_eq!(gaps.all()[0].project, None);
    }

    #[test]
    fn rejects_invalid_json_without_creating_gap() {
        let dir = tempdir().unwrap();
        let host = dir.path().join("host/inbox");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("bad.json"), "{not json").unwrap();
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let report = import_gap_spool_from_dirs(
            &mut gaps,
            &[],
            &dir.path().join("state"),
            Some(host.clone()),
        )
        .unwrap();

        assert_eq!(report.rejected.len(), 1);
        assert!(host.join("rejected/bad.json").exists());
        assert!(host.join("rejected/bad.json.error.txt").exists());
        assert!(gaps.all().is_empty());
    }

    #[test]
    fn skips_duplicate_live_gap_by_dedupe_key() {
        let dir = tempdir().unwrap();
        let host = dir.path().join("host/inbox");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("first.json"), gap_body("Need hook", "hook")).unwrap();
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let first = import_gap_spool_from_dirs(
            &mut gaps,
            &[],
            &dir.path().join("state"),
            Some(host.clone()),
        )
        .unwrap();
        assert_eq!(first.imported.len(), 1);

        fs::write(
            host.join("second.json"),
            gap_body("Need hook again", "hook"),
        )
        .unwrap();
        let second = import_gap_spool_from_dirs(
            &mut gaps,
            &[],
            &dir.path().join("state"),
            Some(host.clone()),
        )
        .unwrap();

        assert_eq!(second.skipped.len(), 1);
        assert_eq!(gaps.all().len(), 1);
        assert!(host.join("imported/second.json").exists());
    }
}
