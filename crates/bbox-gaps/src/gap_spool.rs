use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gaps::{GapNote, GapStore};
use crate::repo_io::{GapRepoCarrier, GapRepoWrite};

const GAP_NOTE_TYPE: &str = "blackbox.gap_note.v1";
// Daemon carrier envelopes encode two separately bounded logical ids plus
// versioned JSON. Keep the envelope bounded without applying the broker's
// per-component limit to the encoded aggregate.
const MAX_CARRIER_ID_BYTES: usize = 4096;

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
            out.push_str(&format!("  skipped {} - {}\n", item.path, item.reason));
        }
        for item in &self.rejected {
            out.push_str(&format!("  rejected {} - {}\n", item.path, item.error));
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
    repo_carrier: Option<GapRepoCarrier>,
    logical_prefix: Option<String>,
    allowed_root: Option<PathBuf>,
}

impl SpoolSource {
    fn repo(root: &Path, carrier: &GapRepoCarrier) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving repository carrier {}", carrier.carrier_id))?;
        Ok(Self {
            inbox_dir: root.join(".bbox").join("gaps").join("inbox"),
            project: Some(carrier.project.clone()),
            repo_carrier: Some(carrier.clone()),
            logical_prefix: Some(format!("carrier:{}/.bbox/gaps/inbox", carrier.carrier_id)),
            allowed_root: Some(root),
        })
    }

    fn host(inbox_dir: PathBuf) -> Self {
        Self {
            inbox_dir,
            project: None,
            repo_carrier: None,
            logical_prefix: None,
            allowed_root: None,
        }
    }

    fn report_path(&self, path: &Path) -> String {
        let Some(prefix) = &self.logical_prefix else {
            return path.display().to_string();
        };
        match path.strip_prefix(&self.inbox_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => prefix.clone(),
            Ok(relative) => format!("{prefix}/{}", relative.display()),
            Err(_) => prefix.clone(),
        }
    }
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

/// Import repository-owned gap spools under operation-scoped mutation
/// authority, then import the host-local spool directly.
///
/// A repository spool import reads files and moves them into `imported/` or
/// `rejected/`, so read-only authority is intentionally insufficient. The
/// authority callback remains active for the complete per-carrier operation.
pub fn import_gap_spool(
    gaps: &mut GapStore,
    project_carriers: &[GapRepoCarrier],
    repo_write: &dyn GapRepoWrite,
    state_dir: &Path,
) -> Result<GapSpoolImportReport> {
    let host_inbox = host_gap_inbox_dir();
    import_gap_spool_with_host_inbox(
        gaps,
        project_carriers,
        repo_write,
        state_dir,
        Some(host_inbox),
    )
}

/// Variant with an explicit host-local inbox. Repository carriers still pass
/// exclusively through `repo_write`; only the host path is accepted directly.
pub fn import_gap_spool_with_host_inbox(
    gaps: &mut GapStore,
    project_carriers: &[GapRepoCarrier],
    repo_write: &dyn GapRepoWrite,
    state_dir: &Path,
    host_inbox: Option<PathBuf>,
) -> Result<GapSpoolImportReport> {
    let state_path = state_dir.join("gap-spool-imports.json");
    let mut state = load_state(&state_path)?;
    let mut report = GapSpoolImportReport::default();

    for carrier in project_carriers {
        validate_spool_carrier(carrier)?;
        let mut invoked = false;
        let mut operation = |root: &Path| {
            if invoked {
                anyhow::bail!(
                    "gap repository mutation authority invoked the operation more than once"
                );
            }
            invoked = true;
            let source = SpoolSource::repo(root, carrier)?;
            import_source(gaps, &mut state, &mut report, &source)
        };
        let authority_result = repo_write.with_write(carrier, &mut operation);
        drop(operation);
        if let Err(err) = authority_result {
            if invoked {
                return Err(err).with_context(|| {
                    format!(
                        "importing gap spool for repository carrier {}",
                        carrier.carrier_id
                    )
                });
            } else {
                tracing::debug!(
                    carrier = %carrier.carrier_id,
                    error = %err,
                    "gap spool skipped repository carrier without mutation authority"
                );
                report.skipped.push(SkippedGapFile {
                    path: format!("carrier:{}/.bbox/gaps/inbox", carrier.carrier_id),
                    reason: "repository mutation authority unavailable".into(),
                });
                continue;
            }
        }
        if !invoked {
            report.skipped.push(SkippedGapFile {
                path: format!("carrier:{}/.bbox/gaps/inbox", carrier.carrier_id),
                reason: "repository mutation authority did not run the operation".into(),
            });
        }
    }

    if let Some(host_inbox) = host_inbox {
        import_source(
            gaps,
            &mut state,
            &mut report,
            &SpoolSource::host(host_inbox),
        )?;
    }

    save_state(&state_path, &state)?;
    Ok(report)
}

fn validate_spool_carrier(carrier: &GapRepoCarrier) -> Result<()> {
    let carrier_id = carrier.carrier_id.as_str();
    if carrier_id.trim().is_empty()
        || carrier_id.len() > MAX_CARRIER_ID_BYTES
        || carrier_id.contains('/')
        || carrier_id.contains('\\')
        || Path::new(carrier_id).is_absolute()
    {
        anyhow::bail!("gap spool carrier must use a bounded path-free logical identifier");
    }
    Ok(())
}

fn import_source(
    gaps: &mut GapStore,
    state: &mut ImportState,
    report: &mut GapSpoolImportReport,
    source: &SpoolSource,
) -> Result<()> {
    let inbox_dir = match source.inbox_dir.canonicalize() {
        Ok(inbox_dir) => inbox_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            report.rejected.push(RejectedGapFile {
                path: source.report_path(&source.inbox_dir),
                moved_to: None,
                error: err.to_string(),
            });
            return Ok(());
        }
    };
    if source
        .allowed_root
        .as_ref()
        .is_some_and(|root| !inbox_dir.starts_with(root))
    {
        anyhow::bail!("gap spool inbox escapes its authorized repository root");
    }
    let source = SpoolSource {
        inbox_dir,
        project: source.project.clone(),
        repo_carrier: source.repo_carrier.clone(),
        logical_prefix: source.logical_prefix.clone(),
        allowed_root: source.allowed_root.clone(),
    };
    let entries = match fs::read_dir(&source.inbox_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.rejected.push(RejectedGapFile {
                path: source.report_path(&source.inbox_dir),
                moved_to: None,
                error: err.to_string(),
            });
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let path = entry.path();
        if !file_type.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        import_one(gaps, state, report, &source, &path)?;
    }
    Ok(())
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
    let path_s = source.report_path(path);
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
    let mut gap = match GapNote::from_envelope(&value, String::new(), bbox_util::util::now_iso()) {
        Ok(gap) => gap,
        Err(err) => {
            reject_file(report, source, path, format!("invalid gap envelope: {err}"))?;
            return Ok(());
        }
    };
    gap.project = source.project.clone();

    let (gap_id, created) = match source.repo_carrier.as_ref() {
        Some(carrier) => gaps.ingest_authorized_carrier(
            gap,
            carrier,
            source
                .allowed_root
                .as_deref()
                .context("repository gap spool has no authorized root")?,
        )?,
        None => gaps.ingest(gap)?,
    };
    let moved_to = move_file(path, &source.inbox_dir.join("imported"), &source.inbox_dir)?;
    if created {
        state.imported.push(ImportedFingerprint {
            path: source.report_path(path),
            sha256: hash,
            gap_id: gap_id.clone(),
            imported_at: bbox_util::util::now_iso(),
        });
        report.imported.push(ImportedGapFile {
            path: source.report_path(path),
            moved_to: source.report_path(&moved_to),
            gap_id,
            project: source.project.clone(),
        });
    } else {
        report.skipped.push(SkippedGapFile {
            path: source.report_path(path),
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
    let moved_to = move_file(path, &source.inbox_dir.join("rejected"), &source.inbox_dir)?;
    let error_path = moved_to.with_extension("json.error.txt");
    fs::write(&error_path, &error).with_context(|| format!("writing {}", error_path.display()))?;
    report.rejected.push(RejectedGapFile {
        path: source.report_path(path),
        moved_to: Some(source.report_path(&moved_to)),
        error,
    });
    Ok(())
}

fn move_file(path: &Path, target_dir: &Path, inbox_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(target_dir).with_context(|| format!("creating {}", target_dir.display()))?;
    let target_dir = target_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", target_dir.display()))?;
    if !target_dir.starts_with(inbox_dir) {
        anyhow::bail!("gap spool destination escapes its authorized inbox");
    }
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
    bbox_corpus_core::json_store::atomic_write_json_locked(path, state)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_io::test_support::TestGapRepoIo;
    use crate::repo_io::{GapRepoRead, GapRepoWrite};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn repo_access(
        project: &Path,
        project_name: &str,
    ) -> (Arc<TestGapRepoIo>, Vec<GapRepoCarrier>) {
        let carrier = GapRepoCarrier::new(project_name, "checkout:test").unwrap();
        let io = Arc::new(TestGapRepoIo::default());
        io.replace(&[(carrier.clone(), project.to_path_buf())]);
        (io, vec![carrier])
    }

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

        let canonical = project.canonicalize().unwrap();
        let project_name = "project:test";
        let (io, carriers) = repo_access(&canonical, project_name);
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let report = import_gap_spool_with_host_inbox(
            &mut gaps,
            &carriers,
            io.as_ref(),
            &dir.path().join("state"),
            None,
        )
        .unwrap();

        assert_eq!(report.imported.len(), 1);
        assert!(inbox.join("imported/gap.json").exists());
        assert_eq!(gaps.all().len(), 1);
        assert_eq!(gaps.all()[0].project.as_deref(), Some(project_name));
        assert_eq!(
            report.imported[0].path,
            "carrier:checkout:test/.bbox/gaps/inbox/gap.json"
        );
    }

    #[test]
    fn checkout_spool_import_leaves_selected_base_bytes_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let base = root.join("base");
        let checkout = root.join("checkout");
        fs::create_dir_all(base.join(".bbox/gaps")).unwrap();
        fs::create_dir_all(checkout.join(".bbox/gaps/inbox")).unwrap();
        let base_gap = base.join(".bbox/gaps/gap-existing.json");
        let base_bytes = br#"{"id":"gap-existing","title":"base stays fixed"}"#;
        fs::write(&base_gap, base_bytes).unwrap();
        fs::write(
            checkout.join(".bbox/gaps/inbox/gap.json"),
            gap_body("Checkout-only gap", "checkout-only"),
        )
        .unwrap();
        let project = base.to_string_lossy().into_owned();
        let carrier = GapRepoCarrier::new(&project, "checkout:session").unwrap();
        let io = TestGapRepoIo::default();
        io.replace(&[(carrier.clone(), checkout.clone())]);
        let mut gaps = GapStore::open(&root.join("gaps.json")).unwrap();

        let report =
            import_gap_spool_with_host_inbox(&mut gaps, &[carrier], &io, &root.join("state"), None)
                .unwrap();

        assert_eq!(report.imported.len(), 1);
        assert_eq!(fs::read(&base_gap).unwrap(), base_bytes);
        assert!(checkout.join(".bbox/gaps/inbox/imported/gap.json").exists());
        assert!(
            checkout
                .join(".bbox/gaps")
                .read_dir()
                .unwrap()
                .any(|entry| {
                    entry.ok().is_some_and(|entry| {
                        entry.file_name().to_string_lossy().starts_with("gap-")
                    })
                })
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

        let (io, carriers) = repo_access(dir.path(), "unused");
        let report = import_gap_spool_with_host_inbox(
            &mut gaps,
            &carriers[..0],
            io.as_ref(),
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

        let (io, carriers) = repo_access(dir.path(), "unused");
        let report = import_gap_spool_with_host_inbox(
            &mut gaps,
            &carriers[..0],
            io.as_ref(),
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

        let (io, carriers) = repo_access(dir.path(), "unused");
        let first = import_gap_spool_with_host_inbox(
            &mut gaps,
            &carriers[..0],
            io.as_ref(),
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
        let second = import_gap_spool_with_host_inbox(
            &mut gaps,
            &carriers[..0],
            io.as_ref(),
            &dir.path().join("state"),
            Some(host.clone()),
        )
        .unwrap();

        assert_eq!(second.skipped.len(), 1);
        assert_eq!(gaps.all().len(), 1);
        assert!(host.join("imported/second.json").exists());
    }

    struct ReadOnlyRepoAuthority;

    impl GapRepoRead for ReadOnlyRepoAuthority {
        fn with_read(
            &self,
            _carrier: &GapRepoCarrier,
            _operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            anyhow::bail!("read authority is not used for spool mutation")
        }
    }

    impl GapRepoWrite for ReadOnlyRepoAuthority {
        fn with_write(
            &self,
            _carrier: &GapRepoCarrier,
            _operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            anyhow::bail!("repository mutation denied")
        }
    }

    #[test]
    fn read_only_authority_cannot_read_or_move_repository_spool() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let inbox = project.join(".bbox/gaps/inbox");
        fs::create_dir_all(&inbox).unwrap();
        let gap_path = inbox.join("gap.json");
        let original = gap_body("Need mutation authority", "write-gate");
        fs::write(&gap_path, &original).unwrap();
        let carrier = GapRepoCarrier::new("project:test", "checkout:denied").unwrap();
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let report = import_gap_spool_with_host_inbox(
            &mut gaps,
            &[carrier],
            &ReadOnlyRepoAuthority,
            &dir.path().join("state"),
            None,
        )
        .unwrap();

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(fs::read_to_string(&gap_path).unwrap(), original);
        assert!(!inbox.join("imported/gap.json").exists());
        assert!(gaps.all().is_empty());
    }

    #[test]
    fn repository_spool_rejects_path_shaped_carrier_identity() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let carrier = GapRepoCarrier::new("project:test", root.to_string_lossy()).unwrap();
        let io = TestGapRepoIo::default();
        io.replace(&[(carrier.clone(), root)]);
        let mut gaps = GapStore::open(&dir.path().join("gaps.json")).unwrap();

        let error = import_gap_spool_with_host_inbox(
            &mut gaps,
            &[carrier],
            &io,
            &dir.path().join("state"),
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("path-free logical identifier"));
        assert!(gaps.all().is_empty());
    }
}
