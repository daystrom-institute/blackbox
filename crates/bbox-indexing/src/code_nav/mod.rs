pub use bbox_code_nav::*;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use bbox_corpus_core::project_record::ProjectRecord;

use crate::index::{first_text, first_u64, optional_text};

#[cfg(test)]
// Code-nav fixtures directly seed and commit an isolated Tantivy index.
#[allow(clippy::disallowed_methods)]
mod tests;

/// Synthesis cases live here AND in `refactor_kind_for` — the two
/// must stay in sync. New synthesis needs an entry in BOTH. The
/// language guard inside the synthesis BooleanQuery mirrors the
/// `(language, symbol_kind, parent_kind)` match in
/// `refactor_kind_for` so a non-Rust grammar that happens to emit a
/// `function_item` under an `impl_item` does not get reported as
/// `impl_method`.
fn indexed_kind_filter_for(
    fields: crate::index::FieldHandles,
    kind: &str,
) -> Vec<Box<dyn tantivy::query::Query>> {
    use tantivy::Term;
    use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
    use tantivy::schema::IndexRecordOption;

    let raw_probe: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_text(fields.symbol_kind, kind),
        IndexRecordOption::Basic,
    ));

    // Synthesis decompositions. Mirror of refactor_kind_for cases —
    // grep "refactor_kind_for" before adding a new case here.
    let synth: Option<Box<dyn Query>> = match kind {
        "impl_method" => Some(Box::new(BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.language, "rust"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.symbol_kind, "function_item"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.parent_kind, "impl_item"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
        ]))),
        _ => None,
    };

    match synth {
        Some(s) => vec![raw_probe, s],
        None => vec![raw_probe],
    }
}

/// Handoff builder for the indexed lane. Mirrors `status_item_handoff`
/// but takes the raw fields stored in tantivy (name/kind/ranges)
/// instead of a `SyntaxItem`, so the indexed lane can produce the
/// exact same shape without re-parsing.
fn indexed_handoff(
    file: &str,
    project_dir: &str,
    language: &str,
    name: Option<&str>,
    refactor_kind: &str,
    byte_range: (usize, usize),
    line_range: (usize, usize),
) -> CodeRefactorHandoff {
    let nearest_refactor_item = Some(CodeNodeSummary {
        kind: refactor_kind.to_string(),
        name: name.map(str::to_string),
        byte_range,
        line_range,
        column_range: (1, 1),
    });
    let refactor_status = name.map(|n| CodeRefactorStatusHint {
        tool: "bbox_refactor_status".to_string(),
        arguments: CodeRefactorStatusHintArgs {
            file: file.to_string(),
            project_dir: Some(project_dir.to_string()),
            item_names: vec![n.to_string()],
            item_kinds: vec![refactor_kind.to_string()],
            limit: 50,
            include_attributes: false,
        },
    });
    let query = name
        .map(str::to_string)
        .or_else(|| Some(refactor_kind.to_string()));
    CodeRefactorHandoff {
        nearest_refactor_item,
        refactor_status,
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                query,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: format!(
            "Indexed code-symbol match for {language}. Same handoff shape as live mode; the indexed lane reads stored project_file docs from tantivy without parsing. Use bbox_refactor_status to confirm exact item names/kinds before planning edits."
        ),
    }
}

/// Dispatcher: `bbox_code_symbols` entry point. Reads `params.mode`
/// and routes to `code_symbols_indexed` or `code_symbols_live`.
///
/// `idx` is `Some` when the call is coming through the MCP handler
/// (which has the daemon's TranscriptIndex). Tests pass `None` to
/// exercise the live lane directly; if such a test asks for
/// `mode="indexed"` it gets a typed error telling it to upgrade.
pub fn code_symbols(
    p: &CodeSymbolSearchParams,
    registered: &[ProjectRecord],
    idx: Option<&crate::index::TranscriptIndex>,
) -> Result<String> {
    // Default mode rules:
    // - Caller explicitly set `mode`: honour it (Indexed requires idx).
    // - Caller left `mode` unset and we have an index: default to
    //   Indexed (fast path).
    // - Caller left `mode` unset and we have no index (test path):
    //   default to Live.
    let mut mode = match p.mode.as_deref() {
        Some(raw) => match CodeSymbolMode::from_param(Some(raw)) {
            Ok(m) => m,
            Err(_) => return err_invalid_code_symbols_mode(raw),
        },
        None if idx.is_some() => CodeSymbolMode::Indexed,
        None => CodeSymbolMode::Live,
    };

    // A managed fleet worktree lives outside the registered repo root, so the
    // registration gate would reject its path. Resolve it to its registered base
    // and add the synthesized alias to the effective project list so both lanes
    // accept the worktree path. The worktree's files are NOT indexed under the
    // base project_id, so the indexed lane would return base-repo-pathed, base-
    // state results — force the live lane instead, which walks the worktree's
    // actual files. This is a more-accurate route, not a degraded fallback, so
    // it overrides an explicit mode=indexed.
    let worktree_alias =
        crate::projects::managed_fleet_worktree_project(Some(&p.project_dir), registered);
    let effective: Vec<ProjectRecord> = match &worktree_alias {
        Some(alias) => registered
            .iter()
            .cloned()
            .chain(std::iter::once(alias.clone()))
            .collect(),
        None => registered.to_vec(),
    };
    if worktree_alias.is_some() {
        mode = CodeSymbolMode::Live;
    }

    match mode {
        CodeSymbolMode::Indexed => match idx {
            Some(index) => code_symbols_indexed(p, &effective, index),
            None => Err(anyhow!(
                "mode=\"indexed\" requires a TranscriptIndex; pass one via the MCP handler or use mode=\"live\""
            )),
        },
        CodeSymbolMode::Live => code_symbols_live(p, &effective),
    }
}

/// Build the JSON for a `invalid_code_symbols_mode` error response.
fn err_invalid_code_symbols_mode(raw: &str) -> Result<String> {
    let response = CodeNavErrorResponse {
        status: "error".to_string(),
        code: "invalid_code_symbols_mode".to_string(),
        message: format!(
            "mode {raw:?} is not valid for bbox_code_symbols; expected \"indexed\" or \"live\""
        ),
        suggestion: "Pass mode=\"indexed\" (default when the daemon has a populated index) or mode=\"live\" to walk and reparse the project tree.".to_string(),
        file: None,
        file_bytes: None,
        max_bytes: None,
        project_dir: None,
        registered_projects: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn code_symbols_indexed(
    p: &CodeSymbolSearchParams,
    registered: &[ProjectRecord],
    idx: &crate::index::TranscriptIndex,
) -> Result<String> {
    use tantivy::collector::TopDocs;
    use tantivy::query::{BooleanQuery, Occur, Query as QueryTrait, TermQuery};
    use tantivy::schema::IndexRecordOption;
    use tantivy::{TantivyDocument, Term};

    let project_dir = PathBuf::from(&p.project_dir)
        .canonicalize()
        .with_context(|| format!("failed to resolve project_dir {}", p.project_dir))?;
    if !project_dir.is_dir() {
        return Err(anyhow!("project_dir must be a directory"));
    }
    if let Some(err_json) = check_project_dir_registered(&project_dir, registered)? {
        return Ok(err_json);
    }

    // Map project_dir → project_id via the registered-roots scan
    // (registered_for the same check above already accepted us).
    let project_id = registered
        .iter()
        .filter(|rec| {
            let root = PathBuf::from(&rec.canonical_path);
            let canon = root.canonicalize().unwrap_or(root);
            project_dir == canon || project_dir.starts_with(&canon)
        })
        // Prefer the deepest registered ancestor when worktrees nest
        // (e.g. .claude/worktrees/foo under transcript-search).
        .max_by_key(|rec| rec.canonical_path.len())
        .map(|rec| rec.project_id.clone())
        .ok_or_else(|| anyhow!("internal: project_dir passed gate but no project_id resolved"))?;
    let project_dir_arg = project_dir.to_string_lossy().into_owned();

    let limit = p.limit.unwrap_or(100).min(1000);
    let language_filter: Vec<String> = p
        .languages
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let kind_filter: Vec<String> = p
        .item_kinds
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let fields = idx.field_handles();
    let mut clauses: Vec<(Occur, Box<dyn QueryTrait>)> = vec![
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.doc_type, "project_file"),
                IndexRecordOption::Basic,
            )),
        ),
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.project_id, &project_id),
                IndexRecordOption::Basic,
            )),
        ),
    ];

    // Languages: union (Should) across the requested set, wrapped in
    // a Must so any-match is required.
    if !language_filter.is_empty() {
        let lang_clauses: Vec<(Occur, Box<dyn QueryTrait>)> = language_filter
            .iter()
            .map(|lang| {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.language, lang),
                        IndexRecordOption::Basic,
                    )) as Box<dyn QueryTrait>,
                )
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(lang_clauses))));
    }

    // Kinds: accept BOTH vocabularies on a single filter list:
    // - raw tree-sitter kinds (e.g. `"function_item"`) match the
    //   stored `symbol_kind` field directly
    // - refactor synthetic kinds (e.g. `"impl_method"`) decompose
    //   into a constraint on (symbol_kind, parent_kind), e.g.
    //   impl_method => symbol_kind=function_item AND
    //                  parent_kind=impl_item
    // The set of synthesis cases lives in `indexed_kind_filter_for`
    // alongside `refactor_kind_for`.
    if !kind_filter.is_empty() {
        let kind_clauses: Vec<(Occur, Box<dyn QueryTrait>)> = kind_filter
            .iter()
            .flat_map(|kind| indexed_kind_filter_for(fields, kind))
            .map(|q| (Occur::Should, q))
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(kind_clauses))));
    }

    let query: Box<dyn QueryTrait> = Box::new(BooleanQuery::new(clauses));
    // Snapshot the shared daemon reader (cheap; avoids the per-call
    // reader-build that codex round-1 review flagged).
    let searcher = idx.searcher();

    // The indexed lane can't push `query`/`path_contains` substring
    // filters into tantivy — they're free-text substrings, not
    // tokenisable. We over-fetch up to a high cap and post-filter; if
    // we hit the cap with `matching_items > items.len()` we set
    // `truncation_reason = "scan_cap_reached"` so the caller knows the
    // count is a lower bound. Without an explicit `query`/path filter
    // the tantivy-level filter is exact, so `limit` itself is the
    // truthful upper bound.
    let has_post_filter = p.query.as_deref().is_some_and(|q| !q.is_empty())
        || p.path_contains.as_deref().is_some_and(|q| !q.is_empty());
    const INDEXED_SCAN_CAP: usize = 5000;

    // Three honest paths:
    //
    // 1. has_post_filter=false: every tantivy hit is a valid match.
    //    Count() gives us the exact total; fetch limit + small
    //    headroom; `truncated = total > items.len()` =>
    //    `limit_reached`. `scan_cap_reached` is NEVER reported on
    //    this path because the fetch ceiling is bound by `limit`,
    //    not by what tantivy could return.
    //
    // 2. has_post_filter=true and tantivy match count <=
    //    INDEXED_SCAN_CAP: fetch all of them. Post-filter walks the
    //    full set; the post-filtered count is exact. Truncation is
    //    `limit_reached` if post-filtered > items.len().
    //
    // 3. has_post_filter=true and tantivy matches > INDEXED_SCAN_CAP:
    //    fetch the cap, walk what we got, and set
    //    `truncation_reason = "scan_cap_reached"` so the caller
    //    knows there may be more matches past the cap and that
    //    `matching_items` is a lower bound.
    use tantivy::collector::Count;
    let total_hits = searcher.search(&*query, &Count)?;
    let fetch_cap = if has_post_filter {
        total_hits.min(INDEXED_SCAN_CAP)
    } else {
        // No post-filter: just enough to fill `limit` + headroom so
        // we have a coverage signal if anything (shouldn't) gets
        // dropped during stored-field load.
        limit.saturating_mul(2).max(64).min(total_hits)
    };
    let hits = searcher.search(&*query, &TopDocs::with_limit(fetch_cap))?;
    let scan_cap_hit = has_post_filter && total_hits > INDEXED_SCAN_CAP;

    let mut items: Vec<CodeSymbolSearchItem> = Vec::new();
    let mut matched_file_paths = std::collections::HashSet::new();
    let mut matching_items = 0usize;

    for (_score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let stored_file_path = first_text(&doc, fields.file_path);
        let rel_path =
            if let Ok(rel) = std::path::Path::new(&stored_file_path).strip_prefix(&project_dir) {
                rel.to_string_lossy().into_owned()
            } else {
                stored_file_path.clone()
            };

        if let Some(path_contains) = p.path_contains.as_deref().filter(|s| !s.is_empty())
            && !rel_path.contains(path_contains)
        {
            continue;
        }

        let language = optional_text(&doc, fields.language).unwrap_or_default();
        let symbol_kind_raw = optional_text(&doc, fields.symbol_kind);
        let parent_kind_raw = optional_text(&doc, fields.parent_kind);
        // Indexed-only docs that predate CN-D3 don't have symbol_kind.
        // Skip them — the live lane is the fallback for pre-reindex
        // states.
        let Some(symbol_kind) = symbol_kind_raw.clone() else {
            continue;
        };
        let refactor_kind = refactor_kind_for(&language, &symbol_kind, parent_kind_raw.as_deref());
        let symbol_display = optional_text(&doc, fields.symbol);
        let symbol_exact = optional_text(&doc, fields.symbol_exact);
        let name = symbol_exact.or(symbol_display.clone());

        // Substring filter on name/path/kind, mirroring live lane.
        if let Some(query) = p.query.as_deref().filter(|s| !s.is_empty()) {
            let in_name = name.as_deref().is_some_and(|n| n.contains(query));
            let in_kind = refactor_kind.contains(query) || symbol_kind.contains(query);
            let in_path = rel_path.contains(query);
            if !(in_name || in_kind || in_path) {
                continue;
            }
        }

        let byte_start = first_u64(&doc, fields.byte_offset) as usize;
        let byte_end = first_u64(&doc, fields.byte_end) as usize;
        let line_start = first_u64(&doc, fields.line_start) as usize;
        let line_end = first_u64(&doc, fields.line_end) as usize;

        matching_items += 1;
        matched_file_paths.insert(rel_path.clone());
        if items.len() >= limit {
            continue;
        }

        let handoff = indexed_handoff(
            &rel_path,
            &project_dir_arg,
            &language,
            name.as_deref(),
            &refactor_kind,
            (byte_start, byte_end),
            (line_start, line_end),
        );

        items.push(CodeSymbolSearchItem {
            file: rel_path,
            language,
            kind: refactor_kind,
            symbol_kind: Some(symbol_kind),
            parent_kind: parent_kind_raw,
            name,
            byte_range: (byte_start, byte_end),
            line_range: (line_start, line_end),
            handoff,
        });
    }

    items.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.byte_range.0.cmp(&b.byte_range.0))
    });

    let truncated = matching_items > items.len() || scan_cap_hit;
    // scan_cap_hit dominates limit_reached because the caller needs
    // to know the count itself is a lower bound — more matches may
    // exist past the cap that we never even inspected.
    let truncation_reason = if scan_cap_hit {
        Some("scan_cap_reached".to_string())
    } else if matching_items > items.len() {
        Some("limit_reached".to_string())
    } else {
        None
    };
    let response = CodeSymbolSearchResponse {
        status: "ok".to_string(),
        project_dir: project_dir_arg,
        mode: CodeSymbolMode::Indexed.label().to_string(),
        // Indexed lane has no per-file scan; report 0 to keep the
        // field present (response shape stable across modes) while
        // signalling the asymmetry honestly.
        scanned_files: 0,
        matched_files: matched_file_paths.len(),
        matching_items,
        returned_items: items.len(),
        truncated,
        truncation_reason,
        items,
        errors: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}
