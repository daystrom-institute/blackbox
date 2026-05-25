//! `java_find_usages` — JDTLS LSP textDocument/references analysis.
//!
//! Resolves binding-aware references to the Java symbol at a given
//! file position. Distinct from the syntax-only `bbox_code_refs` path.
//!
//! RX-V3 fail-closed: a missing or unavailable LSP session returns
//! `error.lsp_unavailable` instead of silently downgrading to a
//! syntactic guess.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lsp_types::{
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, request::References,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::code_nav::{
    CodeProjectRefsHint, CodeProjectRefsHintArgs, CodeRefactorHandoff, CodeRefactorStatusHint,
    CodeRefactorStatusHintArgs,
};
use crate::lsp::LspSessionManager;
use crate::projects::Language;

/// Semantic status value reported by the LSP lane.
pub const SEMANTIC_STATUS_LSP_VERIFIED: &str = "lsp_verified";

/// A single resolved usage site returned by `java_find_usages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSite {
    pub path: String,
    pub line: u32,
    pub character: u32,
    /// Handoff hint pointing the agent to `bbox_refactor_status` /
    /// `bbox_refactor_project_refs` on this specific file.
    pub handoff: CodeRefactorHandoff,
}

/// Top-level report returned by `java_find_usages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsagesReport {
    pub kind: String,
    /// Always `"lsp_verified"` when this report is produced; the function
    /// fails closed rather than downgrading to a syntactic approximation.
    pub semantic_status: String,
    pub source: String,
    /// `true` when JDTLS resolved the symbol at the requested position.
    /// `false` means the LSP returned an empty/null result — "no usages"
    /// vs "couldn't resolve" is ambiguous at the LSP level, so we
    /// conservatively set this to `false` only when the response is `None`.
    pub symbol_resolved: bool,
    pub usage_count: usize,
    pub usages: Vec<UsageSite>,
}

/// Resolve references to the Java symbol at `(line, column)` in
/// `source_path` via JDTLS `textDocument/references`.
///
/// Fail-closed (RX-V3): returns `Err` with an `error.lsp_unavailable`
/// message when the session manager is unavailable or JDTLS fails to
/// initialise. Never returns a syntactic approximation labelled as
/// `lsp_verified`.
///
/// `line` and `column` are **0-based** LSP coordinates.
pub(crate) fn java_find_usages(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    line: u32,
    column: u32,
) -> Result<UsagesReport> {
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;

    // Convert the caller-supplied 0-based (line, column) into an LSP
    // Position.  We construct it directly because byte_to_lsp_position
    // takes a byte offset; the caller already knows the LSP coordinates.
    let position = lsp_types::Position { line, character: column };

    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let response = manager
        .with_session(project_dir, Language::Java, |mut client| {
            // Open the file and wait for diagnostics so JDTLS has indexed
            // and type-checked it before we ask for references — same
            // pattern as jdtls_organize_imports.
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                &lsp_types::DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: source_uri.clone(),
                        language_id: "java".to_string(),
                        version: 0,
                        text: source.clone(),
                    },
                },
            )?;
            client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
            let id = client.send_request::<References>(&params)?;
            client.read_response::<References>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let project_dir_str = project_dir.to_string_lossy().to_string();
    let source_str = source_path.to_string_lossy().to_string();

    let (usages, symbol_resolved) = match response {
        Some(locations) => {
            let usages = locations
                .into_iter()
                .map(|loc| {
                    let path = loc
                        .uri
                        .to_file_path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| loc.uri.to_string());
                    let site_line = loc.range.start.line;
                    let site_char = loc.range.start.character;
                    let handoff = usage_site_handoff(&path, &project_dir_str);
                    UsageSite {
                        path,
                        line: site_line,
                        character: site_char,
                        handoff,
                    }
                })
                .collect();
            (usages, true)
        }
        None => (Vec::new(), false),
    };

    Ok(UsagesReport {
        kind: "java_find_usages".to_string(),
        semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
        source: source_str,
        symbol_resolved,
        usage_count: usages.len(),
        usages,
    })
}

/// Build a minimal `CodeRefactorHandoff` for a single LSP-resolved usage
/// site.  Unlike `refactor_handoff` (which needs a live tree-sitter node),
/// this constructs the handoff from the file path alone — appropriate when
/// we have a location from the LSP but have not re-parsed the file.
fn usage_site_handoff(file: &str, project_dir: &str) -> CodeRefactorHandoff {
    CodeRefactorHandoff {
        nearest_refactor_item: None,
        refactor_status: Some(CodeRefactorStatusHint {
            tool: "bbox_refactor_status".to_string(),
            arguments: CodeRefactorStatusHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                item_names: vec![],
                item_kinds: vec![],
                limit: 50,
                include_attributes: false,
            },
        }),
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                query: None,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: "LSP-resolved Java usage site (semantic_status=lsp_verified). Use bbox_refactor_status to inspect the enclosing item before planning edits; use bbox_refactor_project_refs for current project_file entity refs.".to_string(),
    }
}

/// Resolve a path that may be absolute or project-relative.
pub(crate) fn resolve_path_for_usages(project_dir: Option<&str>, file: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(file);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let base = match project_dir {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting current directory")?,
    };
    Ok(base.join(candidate))
}

/// Resolve `project_dir`: if supplied use it, otherwise walk up from
/// `source_path` to the git root, or fall back to the file's parent.
pub(crate) fn resolve_project_dir_for_usages(
    project_dir: Option<&str>,
    source_path: &Path,
) -> PathBuf {
    project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(source_path)
                .unwrap_or_else(|| {
                    source_path
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_path_buf()
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fail-closed: when no LSP manager is configured (simulated by
    /// constructing a manager that has been shut down immediately), the
    /// function must return an `error.lsp_unavailable` error rather than
    /// silently producing a syntactic guess.
    ///
    /// This is the unit test for RX-V3 compliance on the Java usages path.
    #[test]
    fn fail_closed_when_lsp_unavailable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let source_path = dir.path().join("Foo.java");
        std::fs::write(&source_path, "public class Foo {}\n").unwrap();

        // Build a fresh manager and immediately shut it down so that
        // `with_session` returns an error (shutting_down=true).
        let manager = LspSessionManager::new();
        manager.shutdown_all();

        let err = java_find_usages(&manager, dir.path(), &source_path, 0, 7)
            .expect_err("expected error.lsp_unavailable");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.lsp_unavailable"),
            "expected 'error.lsp_unavailable' in error message, got: {msg}"
        );
    }

    /// Verify UsagesReport serialises to JSON with the expected top-level
    /// fields so downstream agents can rely on the shape.
    #[test]
    fn usages_report_serializes() {
        let report = UsagesReport {
            kind: "java_find_usages".to_string(),
            semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
            source: "/repo/Foo.java".to_string(),
            symbol_resolved: true,
            usage_count: 1,
            usages: vec![UsageSite {
                path: "/repo/Bar.java".to_string(),
                line: 10,
                character: 4,
                handoff: usage_site_handoff("/repo/Bar.java", "/repo"),
            }],
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"lsp_verified\""));
        assert!(json.contains("\"java_find_usages\""));
        assert!(json.contains("\"symbol_resolved\": true"));
        assert!(json.contains("\"usage_count\": 1"));
        assert!(json.contains("bbox_refactor_status"));
    }

    /// Live integration test against a real JDTLS instance.
    ///
    /// Fixture: `tests/fixtures/java/Hello.java`
    ///   - `greet()` declared at 1-based line=3, col=19 → 0-based line=2, col=18
    ///   - Called at line 9 (`h.greet()`) and line 10 (`h.greet()`)
    ///   - `include_declaration=true` so the declaration itself is also returned
    ///   - Expected: >=1 resolved site, semantic_status="lsp_verified"
    ///
    /// Skipped by default (`#[ignore]`) because JDTLS has a ~60s cold start.
    ///
    /// Run with:
    ///   BLACKBOX_JDTLS_BIN=/usr/bin/jdtls cargo test --lib code_nav::semantic::tests::live_jdtls_references -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live JDTLS (/usr/bin/jdtls); ~60s cold start"]
    fn live_jdtls_references() {
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java");
        assert!(
            fixtures_dir.exists(),
            "Java fixture directory missing: {}",
            fixtures_dir.display()
        );

        let source_path = fixtures_dir.join("Hello.java");
        assert!(
            source_path.exists(),
            "Hello.java fixture missing: {}",
            source_path.display()
        );

        // Anchor: `greet` declaration in Hello.java.
        // 1-based: line=3, col=19
        // → 0-based LSP: line=2, col=18  (mirrors the handler's saturating_sub(1))
        let lsp_line: u32 = 3u32.saturating_sub(1); // = 2
        let lsp_col: u32 = 19u32.saturating_sub(1); // = 18

        let manager = LspSessionManager::new();
        let result = java_find_usages(&manager, &fixtures_dir, &source_path, lsp_line, lsp_col);
        match result {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report)
                    .expect("serialise UsagesReport");
                println!("--- live_jdtls_references output ---\n{json}\n---");
                assert_eq!(
                    report.semantic_status, SEMANTIC_STATUS_LSP_VERIFIED,
                    "semantic_status must be lsp_verified"
                );
                assert!(
                    report.symbol_resolved,
                    "symbol_resolved must be true for `greet`; got report: {json}"
                );
                assert!(
                    report.usage_count >= 1,
                    "expected >=1 usage site for `greet`; got {}: {json}",
                    report.usage_count
                );
            }
            Err(e) => {
                panic!("live JDTLS references failed: {e:#}");
            }
        }
        manager.shutdown_all();
    }
}
