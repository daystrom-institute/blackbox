//! Wave-2 SPINE (Drone 1 owns this file + the `agent_loop` seam wiring).
//!
//! Responsibilities:
//! 1. After a mutating tool dispatch, drain the `bro-tools` `EditSink` and, for
//!    each edited file, run `bro-lsp` (open / `didChange` / pull) to get the
//!    fresh `Vec<lsp_types::Diagnostic>`.
//! 2. Diff the fresh pass against the file's baseline in
//!    [`crate::lsp_baselines::LspBaselines`] using a STABLE, line-number-
//!    independent identity (e.g. code + message + normalized span/symbol), so a
//!    diagnostic that merely shifted lines is NOT reported as new.
//! 3. Update the baseline with the fresh pass after diffing.
//! 4. Return one [`DiffResult`] per edited file.
//! 5. Wire this into the edit loop at the post-dispatch seam in
//!    `agent_loop.rs` (~487-530): if the `EditSink` is non-empty, drain →
//!    `check_edits` → `render::build_rider` → append the rider to the tool
//!    result `content` (same append shape as `bound.rs`).
//!
//! Design notes (yours to finalize — you own both ends of this function):
//! - The `bro_lsp::SessionPool` should be LOOP-LIVED (warm sessions reused
//!   across edits), not constructed per call. Thread it through the loop state;
//!   the signature below is a starting point you may refine, as long as you
//!   still return `Vec<DiffResult>` for `render::build_rider`.
//! - Honor drop-stale: pull diagnostics for the version you just applied; a
//!   `Superseded` result must not be surfaced.
//! - MVP scope: Rust + rust-analyzer, error tier. Detect language from the file
//!   extension; skip non-Rust files for now.

use super::DiffResult;
use crate::lsp_baselines::{Baseline, LspBaselines};
use anyhow::{Context, Result};
use bro_lsp::{Language, OpenDocument, SessionPool};
use bro_tools::edits::EditEvent;
use lsp_types::{Diagnostic, NumberOrString, Range};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

const BASELINE_FORMAT: &str = "bro-harness.window0.diagnostics.v1";

#[derive(Debug)]
struct PendingEdit {
    path: PathBuf,
    post_sha256: String,
}

struct DocumentSync<'a> {
    root: &'a Path,
    file: &'a str,
    path: &'a Path,
    baseline_version: u64,
    post_text: String,
}

#[derive(Debug)]
struct BaselineKeys {
    keys: Vec<String>,
    had_explicit_keys: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct BaselinePayload {
    format: String,
    diagnostics: Vec<Diagnostic>,
    keys: Vec<String>,
}

#[derive(Serialize)]
struct DiagnosticKey {
    source: Option<String>,
    code: Option<String>,
    message: String,
    span: SpanKey,
}

#[derive(Serialize)]
struct SpanKey {
    start_character: u32,
    end_character: u32,
    line_span: u32,
    symbol: Option<String>,
}

/// Run window-0 diagnostics for a batch of edits, diff against baselines, and
/// return per-file new/changed findings. Updates `baselines` in place.
pub async fn check_edits(
    edits: &[EditEvent],
    baselines: &mut LspBaselines,
    documents: &mut BTreeMap<String, OpenDocument>,
    pool: &SessionPool,
    root: &Path,
) -> anyhow::Result<Vec<DiffResult>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing diagnostics root {}", root.display()))?;
    let pending = latest_rust_edits(edits, &root)?;
    let mut results = Vec::new();

    for (file, edit) in pending {
        let post_text = tokio::fs::read_to_string(&edit.path)
            .await
            .with_context(|| format!("reading edited Rust file {}", edit.path.display()))?;
        let baseline_version = baselines.files.get(&file).map_or(0, |b| b.version);
        let doc = sync_document(
            pool,
            documents,
            DocumentSync {
                root: &root,
                file: &file,
                path: &edit.path,
                baseline_version,
                post_text: post_text.clone(),
            },
        )
        .await?;

        let fresh = match pool.diagnostics(&doc, doc.version).await {
            Ok(diagnostics) => diagnostics,
            Err(err) if err.is_superseded() => {
                tracing::debug!(file, version = doc.version, error = %err, "dropping stale diagnostics");
                continue;
            }
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("pulling diagnostics for {file}"));
            }
        };

        let previous = baselines.files.get(&file).map(baseline_keys);
        let old_keys = previous
            .as_ref()
            .map(|previous| previous.keys.as_slice())
            .unwrap_or(&[]);
        let compare_with_span_text = previous
            .as_ref()
            .map(|previous| previous.had_explicit_keys)
            .unwrap_or(true);
        let fresh_keys_for_diff =
            diagnostic_keys(&fresh, compare_with_span_text.then_some(post_text.as_str()));
        let diff = diff_diagnostics(&file, old_keys, &fresh_keys_for_diff, &fresh);
        let fresh_keys_for_store = diagnostic_keys(&fresh, Some(&post_text));

        baselines.files.insert(
            file,
            Baseline {
                sha256: edit.post_sha256,
                version: doc.version as u64,
                diagnostics: serde_json::to_value(BaselinePayload {
                    format: BASELINE_FORMAT.to_string(),
                    diagnostics: fresh,
                    keys: fresh_keys_for_store,
                })?,
            },
        );
        results.push(diff);
    }

    Ok(results)
}

fn latest_rust_edits(edits: &[EditEvent], root: &Path) -> Result<BTreeMap<String, PendingEdit>> {
    let mut pending = BTreeMap::new();
    for edit in edits {
        if edit.path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let path = edit
            .path
            .canonicalize()
            .with_context(|| format!("canonicalizing edited file {}", edit.path.display()))?;
        let rel = path
            .strip_prefix(root)
            .with_context(|| {
                format!(
                    "edited file {} is outside diagnostics root {}",
                    path.display(),
                    root.display()
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        pending.insert(
            rel,
            PendingEdit {
                path,
                post_sha256: edit.post_sha256.clone(),
            },
        );
    }
    Ok(pending)
}

async fn sync_document(
    pool: &SessionPool,
    documents: &mut BTreeMap<String, OpenDocument>,
    sync: DocumentSync<'_>,
) -> Result<OpenDocument> {
    if let Some(doc) = documents.get_mut(sync.file) {
        let version = next_version(doc.version as u64, sync.baseline_version)?;
        pool.apply_change(doc, version, sync.post_text).await?;
        return Ok(doc.clone());
    }

    let open_version = next_version(0, sync.baseline_version)?;
    let doc = pool
        .open_document(
            sync.root,
            Language::Rust,
            sync.path,
            open_version,
            sync.post_text,
        )
        .await?;
    documents.insert(sync.file.to_string(), doc.clone());
    Ok(doc)
}

fn next_version(current_doc_version: u64, baseline_version: u64) -> Result<i32> {
    let next = current_doc_version.max(baseline_version).saturating_add(1);
    i32::try_from(next).context("LSP document version overflow")
}

fn baseline_keys(baseline: &Baseline) -> BaselineKeys {
    if let Ok(payload) = serde_json::from_value::<BaselinePayload>(baseline.diagnostics.clone())
        && payload.keys.len() == payload.diagnostics.len()
    {
        return BaselineKeys {
            keys: payload.keys,
            had_explicit_keys: true,
        };
    }

    let diagnostics =
        serde_json::from_value::<Vec<Diagnostic>>(baseline.diagnostics.clone()).unwrap_or_default();
    BaselineKeys {
        keys: diagnostic_keys(&diagnostics, None),
        had_explicit_keys: false,
    }
}

fn diagnostic_keys(diagnostics: &[Diagnostic], text: Option<&str>) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic_key(diagnostic, text))
        .collect()
}

fn diff_diagnostics(
    file: &str,
    old_keys: &[String],
    fresh_keys: &[String],
    fresh: &[Diagnostic],
) -> DiffResult {
    let mut remaining_old = counts(old_keys);
    let mut new = Vec::new();
    let mut carried = 0;

    for (key, diagnostic) in fresh_keys.iter().zip(fresh) {
        match remaining_old.get_mut(key) {
            Some(count) if *count > 0 => {
                *count -= 1;
                carried += 1;
            }
            _ => new.push(diagnostic.clone()),
        }
    }

    let fixed = remaining_old.values().sum();
    DiffResult {
        file: file.to_string(),
        new,
        fixed,
        carried,
    }
}

fn counts(keys: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for key in keys {
        *counts.entry(key.clone()).or_default() += 1;
    }
    counts
}

fn diagnostic_key(diagnostic: &Diagnostic, text: Option<&str>) -> String {
    let range = diagnostic.range;
    let key = DiagnosticKey {
        source: diagnostic.source.clone(),
        code: diagnostic.code.as_ref().map(code_key),
        message: normalize_text(&diagnostic.message),
        span: SpanKey {
            start_character: range.start.character,
            end_character: range.end.character,
            line_span: range.end.line.saturating_sub(range.start.line),
            symbol: text.and_then(|text| symbol_for_range(text, range)),
        },
    };
    serde_json::to_string(&key).expect("diagnostic key serialization is infallible")
}

fn code_key(code: &NumberOrString) -> String {
    match code {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.clone(),
    }
}

fn symbol_for_range(text: &str, range: Range) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start_line = lines.get(range.start.line as usize)?;
    let symbol = if range.start.line == range.end.line {
        let start = byte_index_for_character(start_line, range.start.character);
        let end = byte_index_for_character(start_line, range.end.character);
        if end > start {
            start_line[start..end].to_string()
        } else {
            token_at(start_line, start)
        }
    } else {
        multiline_range_text(&lines, range)?
    };
    let symbol = normalize_text(&symbol);
    (!symbol.is_empty()).then(|| truncate_chars(&symbol, 120))
}

fn multiline_range_text(lines: &[&str], range: Range) -> Option<String> {
    let start_idx = range.start.line as usize;
    let end_idx = range.end.line as usize;
    let mut parts = Vec::new();
    for idx in start_idx..=end_idx {
        let line = *lines.get(idx)?;
        if idx == start_idx {
            let start = byte_index_for_character(line, range.start.character);
            parts.push(line[start..].to_string());
        } else if idx == end_idx {
            let end = byte_index_for_character(line, range.end.character);
            parts.push(line[..end].to_string());
        } else {
            parts.push(line.to_string());
        }
    }
    Some(parts.join("\n"))
}

fn byte_index_for_character(line: &str, character: u32) -> usize {
    line.char_indices()
        .nth(character as usize)
        .map(|(idx, _)| idx)
        .unwrap_or(line.len())
}

fn token_at(line: &str, byte: usize) -> String {
    let bytes = line.as_bytes();
    let mut start = byte.min(bytes.len());
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = byte.min(bytes.len());
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    line[start..end].to_string()
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        truncated
    } else {
        text.to_string()
    }
}

#[cfg(test)]
// Filesystem/process fixtures intentionally exercise diagnostics baselines.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use lsp_types::DiagnosticSeverity;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_edits_tracks_new_fixed_and_line_shifted_diagnostics() -> Result<()> {
        let Some(rust_analyzer) = rust_analyzer_bin() else {
            eprintln!("skipping diagnostics engine test: rust-analyzer not found");
            return Ok(());
        };

        let dir = TestDir::new()?;
        let root = dir.path().canonicalize()?;
        std::fs::create_dir(root.join("src"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "bro_harness_diag_fixture"
version = "0.1.0"
edition = "2024"
"#,
        )?;

        let source_path = root.join("src/lib.rs");
        let clean = r#"pub fn value() -> u32 {
    let x: u32 = 1;
    x
}
"#;
        let broken = r#"pub fn value() -> u32 {
    let x: u32 = "s";
    x
}
"#;
        let shifted = format!("// shifted down\n{broken}");
        std::fs::write(&source_path, clean)?;

        let pool = SessionPool::new(bro_lsp::LspConfig {
            request_timeout: Duration::from_secs(90),
            init_timeout: Duration::from_secs(90),
            ready_timeout: Duration::from_secs(5),
            rust_analyzer_bin: Some(rust_analyzer),
            ..bro_lsp::LspConfig::default()
        });
        let mut baselines = LspBaselines::default();
        let mut documents = BTreeMap::new();

        std::fs::write(&source_path, broken)?;
        let diffs = check_edits(
            &[EditEvent::from_bytes(
                source_path.clone(),
                clean.as_bytes(),
                broken.as_bytes(),
            )],
            &mut baselines,
            &mut documents,
            &pool,
            &root,
        )
        .await?;
        assert_eq!(diffs.len(), 1);
        assert!(
            diffs[0]
                .new
                .iter()
                .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR)),
            "expected a new rust-analyzer error, got {diffs:#?}"
        );

        std::fs::write(&source_path, &shifted)?;
        let shifted_diffs = check_edits(
            &[EditEvent::from_bytes(
                source_path.clone(),
                broken.as_bytes(),
                shifted.as_bytes(),
            )],
            &mut baselines,
            &mut documents,
            &pool,
            &root,
        )
        .await?;
        assert_eq!(shifted_diffs.len(), 1);
        assert!(
            shifted_diffs[0].new.is_empty(),
            "line-shifted diagnostic should carry, not become new: {shifted_diffs:#?}"
        );
        assert!(
            shifted_diffs[0].carried > 0,
            "expected shifted diagnostic to be carried: {shifted_diffs:#?}"
        );

        std::fs::write(&source_path, clean)?;
        let fixed_diffs = check_edits(
            &[EditEvent::from_bytes(
                source_path.clone(),
                shifted.as_bytes(),
                clean.as_bytes(),
            )],
            &mut baselines,
            &mut documents,
            &pool,
            &root,
        )
        .await?;
        assert_eq!(fixed_diffs.len(), 1);
        assert!(fixed_diffs[0].new.is_empty(), "{fixed_diffs:#?}");
        assert!(
            fixed_diffs[0].fixed > 0,
            "expected the previous diagnostic to be fixed: {fixed_diffs:#?}"
        );

        pool.shutdown_all().await;
        Ok(())
    }

    fn rust_analyzer_bin() -> Option<PathBuf> {
        if let Some(path) = env_path("BRO_LSP_RUST_ANALYZER_BIN")
            .or_else(|| env_path("BRO_RUST_ANALYZER_BIN"))
            .or_else(|| env_path("BLACKBOX_RUST_ANALYZER_BIN"))
            .filter(|path| command_runs(path))
        {
            return Some(path);
        }

        if command_runs(Path::new("rust-analyzer")) {
            return Some(PathBuf::from("rust-analyzer"));
        }

        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let cargo_bin = home.join(".cargo/bin/rust-analyzer");
        command_runs(&cargo_bin).then_some(cargo_bin)
    }

    fn command_runs(path: &Path) -> bool {
        std::process::Command::new(path)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn env_path(key: &str) -> Option<PathBuf> {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Result<Self> {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("bro-harness-diag-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&path)?;
            Ok(Self { path })
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
