//! Read-only jOOQ-aware Java refactor plan kinds.
//!
//! All planners in this module are analysis-only — they return pretty JSON
//! reports, never mutate a [`RefactorPlan`], and stop at
//! `semantic_status = syntax_only`. They classify only what is visible in
//! source — visible qualifier annotations, literal provider/field text,
//! imports, and method-name shape — and refuse to infer read/write safety,
//! schema policy, soft-delete semantics, or transaction intent.
//!
//! Six functions, all keyed by `RefactorPlanParams`:
//!
//! * [`plan_java_jooq_codegen_config_inventory`] — project-level inventory of
//!   Gradle/Maven/standalone jOOQ generator configuration. Scans
//!   `p.project_dir` and reports the files that look like jOOQ codegen
//!   configuration; per-file extracted hints stay literal.
//! * [`plan_java_jooq_query_structure_analysis`] — per-file (or multi-file via
//!   `p.sources`) analysis of jOOQ fluent chains: `select`, `selectFrom`,
//!   `fetch*`, `Records.mapping`, `DSL.multiset`, `transaction*`, raw `DSL`
//!   fragments, and the visible `DSLContext` receiver.
//! * [`plan_java_jooq_dsl_context_audit`] — inventory of visible
//!   `DSLContext`/`Provider<DSLContext>` fields and parameters with their
//!   adjacent annotations, plus `transaction*` callback sites and nested
//!   transaction shapes. Never asserts read/write or schema safety; the JSON
//!   carries explicit `leftovers` / `caveats` strings.
//! * [`plan_java_jooq_projection_mapping_analysis`] — `Records.mapping(...)`
//!   targets with constructor-reference / record-target shape and the count
//!   of `multiset(...)` nests and selected fields visible in the same chain.
//! * [`plan_java_jooq_raw_sql_fragment_audit`] — stringly-typed `DSL.field(`,
//!   `DSL.table(`, `DSL.condition(` calls, classified as `name_expression`,
//!   `dynamic_sql`, `generated_replacement_candidate`, or
//!   `ambiguous_operator_review`.
//! * [`plan_java_jooq_generated_record_boundary_analysis`] — return-type
//!   signatures and fields/imports that look like generated jOOQ `*Record`
//!   types or `.jooq.tables.records` imports crossing UI/API boundaries.
//!
//! v1 is intentionally syntax/textual. Tree-sitter is used for the method
//! chain walk in `query_structure_analysis`; everywhere else the analyzers
//! scan parsed source line-by-line and surface literal evidence. Confidence
//! never exceeds `medium`.

use super::*;
use std::collections::BTreeSet;

const SEMANTIC_STATUS: &str = "syntax_only";

// ===========================================================================
// Plan kind: java_jooq_codegen_config_inventory
// ===========================================================================

pub(crate) fn plan_java_jooq_codegen_config_inventory(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = resolve_codegen_root(p)?;

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut leftovers: Vec<String> = Vec::new();

    for entry in walkdir::WalkDir::new(&project_dir)
        .max_depth(8)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some(".git" | "target" | "build" | ".gradle" | "node_modules"),
            )
        }) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let looks_like_codegen = matches!(
            name,
            "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
                | "pom.xml",
        ) || (path.extension().and_then(|e| e.to_str()) == Some("xml")
            && path.to_string_lossy().to_lowercase().contains("jooq"));

        if !looks_like_codegen {
            continue;
        }

        let Ok(text) = fs::read_to_string(path) else {
            leftovers.push(format!(
                "could not read {}; codegen evidence not collected",
                path_string(path)
            ));
            continue;
        };

        let lower = text.to_lowercase();
        let mentions_jooq = lower.contains("jooq");
        if !mentions_jooq {
            continue;
        }

        let hints = collect_codegen_hints(&text);
        if hints.is_empty() {
            // File mentions jOOQ but no concrete configuration tags were
            // extracted. Surface anyway with a leftover-style caveat.
            leftovers.push(format!(
                "{} mentions jooq but no extractable generator stanza was \
                 recognized; manual review recommended",
                path_string(path)
            ));
        }

        entries.push(serde_json::json!({
            "file": path_string(path),
            "filename": name,
            "format": guess_codegen_format(name, path),
            "hints": hints,
        }));
    }

    entries.sort_by(|a, b| {
        a.get("file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("file").and_then(|v| v.as_str()).unwrap_or(""))
    });

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_codegen_config_inventory",
        "title": format!(
            "jOOQ codegen configuration inventory under {}",
            path_string(&project_dir)
        ),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "project_dir": path_string(&project_dir),
        "entries": entries,
        "leftovers": leftovers,
        "caveats": [
            "Inventory is textual: forced-type/converter/binding lists are echoed \
             from configuration files but their effective resolution against the \
             generator at codegen time is not verified here.",
            "Generated source ownership is reported as filesystem hints only; \
             confirm against the actual build outputs before relying on them."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

fn resolve_codegen_root(p: &RefactorPlanParams) -> Result<PathBuf> {
    if let Some(dir) = p.project_dir.as_deref().filter(|s| !s.is_empty()) {
        let pb = PathBuf::from(dir);
        if !pb.is_dir() {
            bail!("project_dir `{}` is not a directory", pb.display());
        }
        return Ok(pb);
    }
    if !p.source.is_empty() {
        let src = resolve_path(None, &p.source)?;
        if let Some(parent) = src.parent() {
            if parent.is_dir() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    bail!(
        "java_jooq_codegen_config_inventory requires project_dir or a source file \
         with a parent directory"
    );
}

fn guess_codegen_format(name: &str, path: &Path) -> &'static str {
    match name {
        "build.gradle" => "gradle_groovy",
        "build.gradle.kts" => "gradle_kotlin",
        "settings.gradle" => "gradle_groovy_settings",
        "settings.gradle.kts" => "gradle_kotlin_settings",
        "pom.xml" => "maven_pom",
        _ if path.extension().and_then(|e| e.to_str()) == Some("xml") => "jooq_xml_config",
        _ => "unknown",
    }
}

/// Conservative text extraction: pull a small set of generator-relevant
/// tokens line-by-line. We never interpret these — we surface the raw
/// values so operators can confirm them.
fn collect_codegen_hints(text: &str) -> Vec<serde_json::Value> {
    let mut hints: Vec<serde_json::Value> = Vec::new();

    let interesting_xml_tags = [
        "name",
        "inputSchema",
        "outputSchema",
        "outputCatalog",
        "packageName",
        "directory",
        "generator",
        "database",
        "forcedType",
        "converter",
        "binding",
        "customType",
        "strategy",
        "jdbc",
    ];
    let interesting_gradle_tokens = [
        "jooq {",
        "configuration {",
        "generator {",
        "forcedType",
        "forcedTypes",
        "converter",
        "binding",
        "strategy",
        "schemata",
        "inputSchema",
        "outputSchema",
        "packageName",
        "directory =",
        "name =",
    ];

    for (idx, raw_line) in text.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with("<!--") {
            continue;
        }

        // XML tag heuristic: prefer the *first* opening tag on the line so
        // that `<forcedType><name>uuid</name></forcedType>` records as
        // `forcedType`, not as the inner `name` closure.
        let mut matched_xml: Option<&str> = None;
        for tag in interesting_xml_tags {
            let open = format!("<{tag}>");
            let open_attr = format!("<{tag} ");
            if trimmed.starts_with(&open) || trimmed.starts_with(&open_attr) {
                matched_xml = Some(tag);
                break;
            }
        }
        if matched_xml.is_none() {
            for tag in interesting_xml_tags {
                let close = format!("</{tag}>");
                if trimmed.contains(&close) {
                    matched_xml = Some(tag);
                    break;
                }
            }
        }
        if let Some(tag) = matched_xml {
            hints.push(serde_json::json!({
                "line": idx + 1,
                "kind": "xml_tag",
                "tag": tag,
                "text": truncate_for_hint(trimmed),
            }));
        }

        // Gradle DSL heuristic
        for token in interesting_gradle_tokens {
            if trimmed.contains(token) {
                hints.push(serde_json::json!({
                    "line": idx + 1,
                    "kind": "gradle_token",
                    "token": token,
                    "text": truncate_for_hint(trimmed),
                }));
                break;
            }
        }
    }

    hints
}

fn truncate_for_hint(s: &str) -> String {
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s.to_string()
    }
}

// ===========================================================================
// Plan kind: java_jooq_query_structure_analysis
// ===========================================================================

pub(crate) fn plan_java_jooq_query_structure_analysis(p: &RefactorPlanParams) -> Result<String> {
    let per_file = analyze_each_source(p, analyze_query_structure_in)?;
    let merged: Vec<serde_json::Value> = per_file
        .iter()
        .flat_map(|(src, findings)| {
            findings.iter().map(move |f| {
                serde_json::json!({
                    "source": src,
                    "finding": f,
                })
            })
        })
        .collect();

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_query_structure_analysis",
        "title": multi_title("jOOQ query structure analysis", &per_file),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "files": per_file
            .iter()
            .map(|(src, findings)| serde_json::json!({
                "source": src,
                "findings": findings,
            }))
            .collect::<Vec<_>>(),
        "findings": merged,
        "caveats": [
            "Chain detection is textual + tree-sitter only; nullability, fetch \
             cardinality, and SQL semantics are NOT verified.",
            "Aliases/joins are surfaced when the method name is recognized; \
             receiver-type inference is not performed."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryChainFinding {
    line: usize,
    column: usize,
    snippet: String,
    methods: Vec<String>,
    terminal: Option<String>,
    mapping_target: Option<String>,
    multiset_uses: usize,
    raw_fragment_uses: usize,
    receiver_text: String,
    transaction_wrapping: Option<String>,
}

fn analyze_query_structure_in(
    _source_path: &str,
    parsed: &ParsedSource,
) -> Result<Vec<QueryChainFinding>> {
    if parsed.language != "java" {
        bail!("java_jooq_query_structure_analysis only supports java files");
    }
    let mut out: Vec<QueryChainFinding> = Vec::new();
    let root = parsed.tree.root_node();
    collect_query_chains(root, &parsed.source, false, None, &mut out);
    out.sort_by_key(|f| (f.line, f.column));
    Ok(out)
}

fn collect_query_chains(
    node: Node<'_>,
    source: &str,
    in_tx: bool,
    tx_kind: Option<&str>,
    out: &mut Vec<QueryChainFinding>,
) {
    let (next_in_tx, next_tx_kind) = if node.kind() == "method_invocation" {
        if let Some(name) = method_name_text(node, source) {
            match name {
                "transaction" => (true, Some("transaction")),
                "transactionResult" => (true, Some("transactionResult")),
                _ => (in_tx, tx_kind),
            }
        } else {
            (in_tx, tx_kind)
        }
    } else {
        (in_tx, tx_kind)
    };

    // Identify chain heads: a method_invocation whose receiver is *not*
    // itself a method_invocation, OR whose entire dotted prefix is a
    // recognizable jOOQ entry point.
    if node.kind() == "method_invocation"
        && is_chain_head(node)
        && looks_like_jooq_chain(node, source)
    {
        if let Some(finding) = build_chain_finding(node, source, next_tx_kind) {
            out.push(finding);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_query_chains(child, source, next_in_tx, next_tx_kind, out);
    }
}

/// A chain head is the outermost (right-most) method invocation in a
/// dotted chain — i.e. it is NOT itself the receiver (`object` field) of
/// another method invocation.
fn is_chain_head(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    if parent.kind() != "method_invocation" {
        return true;
    }
    parent.child_by_field_name("object").map(|n| n.id()) != Some(node.id())
}

fn looks_like_jooq_chain(node: Node<'_>, source: &str) -> bool {
    let methods = chain_method_names(node, source);
    methods.iter().any(|m| {
        matches!(
            m.as_str(),
            "select"
                | "selectFrom"
                | "selectDistinct"
                | "selectCount"
                | "selectOne"
                | "fetch"
                | "fetchOne"
                | "fetchOptional"
                | "fetchInto"
                | "fetchAny"
                | "fetchSingle"
                | "fetchMap"
                | "fetchSet"
                | "fetchGroups"
                | "fetchStream"
        )
    }) || chain_uses_records_mapping(node, source)
        || chain_uses_dsl_multiset(node, source)
}

/// Walk a chain head's dotted prefix and return every method name in
/// receiver-to-tail order.
fn chain_method_names(head: Node<'_>, source: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut cur = Some(head);
    let mut stack: Vec<Node<'_>> = Vec::new();
    while let Some(n) = cur {
        if n.kind() != "method_invocation" {
            break;
        }
        stack.push(n);
        cur = n.child_by_field_name("object");
    }
    while let Some(n) = stack.pop() {
        if let Some(name) = method_name_text(n, source) {
            names.push(name.to_string());
        }
    }
    names
}

fn chain_uses_records_mapping(head: Node<'_>, source: &str) -> bool {
    chain_argument_text(head, source).contains("Records.mapping")
}

fn chain_uses_dsl_multiset(head: Node<'_>, source: &str) -> bool {
    let text = chain_argument_text(head, source);
    text.contains("multiset(") || text.contains("DSL.multiset")
}

fn chain_argument_text(head: Node<'_>, source: &str) -> String {
    let start = head.start_byte();
    let end = head.end_byte();
    source[start..end].to_string()
}

fn build_chain_finding(
    head: Node<'_>,
    source: &str,
    tx_kind: Option<&str>,
) -> Option<QueryChainFinding> {
    let methods = chain_method_names(head, source);
    let terminal = methods
        .iter()
        .rev()
        .find(|m| {
            matches!(
                m.as_str(),
                "fetch"
                    | "fetchOne"
                    | "fetchOptional"
                    | "fetchInto"
                    | "fetchAny"
                    | "fetchSingle"
                    | "fetchMap"
                    | "fetchSet"
                    | "fetchGroups"
                    | "fetchStream"
                    | "execute"
            )
        })
        .cloned();

    // Walk down to the inner-most receiver to capture the chain origin.
    let mut cur = head;
    while let Some(next) = cur.child_by_field_name("object") {
        if next.kind() != "method_invocation" {
            // The receiver is the chain root (an identifier, field_access,
            // or class literal).
            let receiver = next
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string();
            let (line, column) = call_site_line_col(head, source);
            let snippet = snippet_for_node(head, source);
            let chain_text = chain_argument_text(head, source);
            let mapping_target = extract_mapping_target(&chain_text);
            let multiset_uses = chain_text.matches("multiset(").count()
                + chain_text.matches("DSL.multiset").count();
            let raw_fragment_uses = count_raw_fragments(&chain_text);
            return Some(QueryChainFinding {
                line,
                column,
                snippet,
                methods,
                terminal,
                mapping_target,
                multiset_uses,
                raw_fragment_uses,
                receiver_text: receiver,
                transaction_wrapping: tx_kind.map(str::to_string),
            });
        }
        cur = next;
    }
    None
}

fn extract_mapping_target(chain_text: &str) -> Option<String> {
    // `Records.mapping(Foo::new)` → `Foo::new`
    if let Some(idx) = chain_text.find("Records.mapping(") {
        let after = &chain_text[idx + "Records.mapping(".len()..];
        let mut depth = 1;
        let mut end = 0;
        for (i, ch) in after.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > 0 {
            return Some(after[..end].trim().to_string());
        }
    }
    None
}

fn count_raw_fragments(chain_text: &str) -> usize {
    let mut count = 0;
    for needle in ["DSL.field(\"", "DSL.table(\"", "DSL.condition(\""] {
        count += chain_text.matches(needle).count();
    }
    count
}

// ===========================================================================
// Plan kind: java_jooq_dsl_context_audit
// ===========================================================================

pub(crate) fn plan_java_jooq_dsl_context_audit(p: &RefactorPlanParams) -> Result<String> {
    let per_file = analyze_each_source(p, audit_dsl_context_in)?;

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_dsl_context_audit",
        "title": multi_title("jOOQ DSLContext audit", &per_file),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "files": per_file.iter().map(|(src, findings)| serde_json::json!({
            "source": src,
            "findings": findings,
        })).collect::<Vec<_>>(),
        "leftovers": [
            "Read/write safety is NEVER inferred from method names, table \
             names, schema names, or statement shape. Only visible qualifier \
             annotations and literal provider/field text are reported.",
            "Cross-file context flow is out of scope; helpers that accept a \
             DSLContext are surfaced as parameters only, not traced into."
        ],
        "caveats": [
            "Nested transactions are flagged when a `transaction`/`transactionResult` \
             call appears inside another such callback; the audit does NOT decide \
             whether nesting is intended.",
            "Visible qualifier annotations are echoed verbatim; the operator owns \
             interpretation."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "finding_kind", rename_all = "snake_case")]
enum DslContextFinding {
    DslContextField {
        line: usize,
        column: usize,
        variable: String,
        type_text: String,
        annotations: Vec<String>,
        is_provider: bool,
    },
    DslContextParameter {
        line: usize,
        column: usize,
        variable: String,
        type_text: String,
        method_name: Option<String>,
        annotations: Vec<String>,
        is_provider: bool,
    },
    ProviderGet {
        line: usize,
        column: usize,
        snippet: String,
        recommendation: String,
    },
    TransactionCallback {
        line: usize,
        column: usize,
        kind: String,
        snippet: String,
    },
    NestedTransaction {
        line: usize,
        column: usize,
        outer_kind: String,
        inner_kind: String,
        snippet: String,
    },
}

fn audit_dsl_context_in(_source: &str, parsed: &ParsedSource) -> Result<Vec<DslContextFinding>> {
    if parsed.language != "java" {
        bail!("java_jooq_dsl_context_audit only supports java files");
    }
    let mut out: Vec<DslContextFinding> = Vec::new();
    let root = parsed.tree.root_node();
    walk_dsl_audit(root, &parsed.source, None, &mut out);
    sort_dsl_findings(&mut out);
    Ok(out)
}

fn sort_dsl_findings(v: &mut [DslContextFinding]) {
    v.sort_by_key(|f| match f {
        DslContextFinding::DslContextField { line, column, .. }
        | DslContextFinding::DslContextParameter { line, column, .. }
        | DslContextFinding::ProviderGet { line, column, .. }
        | DslContextFinding::TransactionCallback { line, column, .. }
        | DslContextFinding::NestedTransaction { line, column, .. } => (*line, *column),
    });
}

fn walk_dsl_audit(
    node: Node<'_>,
    source: &str,
    outer_tx: Option<String>,
    out: &mut Vec<DslContextFinding>,
) {
    let kind = node.kind();

    if kind == "field_declaration" {
        collect_dsl_field(node, source, out);
    }
    if kind == "formal_parameter" {
        collect_dsl_parameter(node, source, out);
    }
    if kind == "method_invocation" {
        if let Some(name) = method_name_text(node, source) {
            if name == "get" {
                if receiver_looks_like_provider(node, source) {
                    let (line, column) = call_site_line_col(node, source);
                    out.push(DslContextFinding::ProviderGet {
                        line,
                        column,
                        snippet: snippet_for_node(node, source),
                        recommendation:
                            "Provider<DSLContext>.get() bypasses transaction callbacks; \
                             use the callback-owned `Configuration.dsl()` when inside a \
                             transaction body."
                                .to_string(),
                    });
                }
            }
            if name == "transaction" || name == "transactionResult" {
                let (line, column) = call_site_line_col(node, source);
                let snippet = snippet_for_node(node, source);
                if let Some(outer) = outer_tx.as_deref() {
                    out.push(DslContextFinding::NestedTransaction {
                        line,
                        column,
                        outer_kind: outer.to_string(),
                        inner_kind: name.to_string(),
                        snippet: snippet.clone(),
                    });
                }
                out.push(DslContextFinding::TransactionCallback {
                    line,
                    column,
                    kind: name.to_string(),
                    snippet,
                });
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    walk_dsl_audit(child, source, Some(name.to_string()), out);
                }
                return;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_dsl_audit(child, source, outer_tx.clone(), out);
    }
}

fn collect_dsl_field(decl: Node<'_>, source: &str, out: &mut Vec<DslContextFinding>) {
    let Some(type_node) = decl.child_by_field_name("type") else {
        return;
    };
    let Ok(type_text_raw) = type_node.utf8_text(source.as_bytes()) else {
        return;
    };
    let type_text = type_text_raw.trim().to_string();
    let (is_dsl, is_provider) = classify_dsl_type(&type_text);
    if !is_dsl {
        return;
    }
    let annotations = collect_preceding_annotations(decl, source);
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                    let (line, column) = line_col(source, name_node.start_byte());
                    out.push(DslContextFinding::DslContextField {
                        line,
                        column,
                        variable: name.to_string(),
                        type_text: type_text.clone(),
                        annotations: annotations.clone(),
                        is_provider,
                    });
                }
            }
        }
    }
}

fn collect_dsl_parameter(param: Node<'_>, source: &str, out: &mut Vec<DslContextFinding>) {
    let Some(type_node) = param.child_by_field_name("type") else {
        return;
    };
    let Ok(type_text_raw) = type_node.utf8_text(source.as_bytes()) else {
        return;
    };
    let type_text = type_text_raw.trim().to_string();
    let (is_dsl, is_provider) = classify_dsl_type(&type_text);
    if !is_dsl {
        return;
    }
    let Some(name_node) = param.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
        return;
    };
    let (line, column) = line_col(source, name_node.start_byte());

    // Annotations live as named children with kind `marker_annotation` /
    // `annotation` directly on the parameter node.
    let mut annotations: Vec<String> = Vec::new();
    let mut cursor = param.walk();
    for child in param.named_children(&mut cursor) {
        if matches!(child.kind(), "annotation" | "marker_annotation") {
            if let Ok(t) = child.utf8_text(source.as_bytes()) {
                annotations.push(t.trim().to_string());
            }
        }
    }

    let method_name = enclosing_method_name(param, source);
    out.push(DslContextFinding::DslContextParameter {
        line,
        column,
        variable: name.to_string(),
        type_text,
        method_name,
        annotations,
        is_provider,
    });
}

/// Returns `(is_dsl_context, is_provider)`.
/// Accepts: `DSLContext`, `org.jooq.DSLContext`, `Provider<DSLContext>`,
/// `jakarta.inject.Provider<DSLContext>`, `javax.inject.Provider<DSLContext>`.
fn classify_dsl_type(text: &str) -> (bool, bool) {
    let t = text.trim();
    if t == "DSLContext" || t.ends_with(".DSLContext") {
        return (true, false);
    }
    let is_provider = t.starts_with("Provider<")
        || t.starts_with("jakarta.inject.Provider<")
        || t.starts_with("javax.inject.Provider<");
    if is_provider
        && (t.contains("<DSLContext>") || t.contains(".DSLContext>") || t.contains("<DSLContext "))
    {
        return (true, true);
    }
    (false, false)
}

fn collect_preceding_annotations(decl: Node<'_>, source: &str) -> Vec<String> {
    let mut annotations: Vec<String> = Vec::new();
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "modifiers" {
            let mut inner = child.walk();
            for m in child.named_children(&mut inner) {
                if matches!(m.kind(), "annotation" | "marker_annotation") {
                    if let Ok(t) = m.utf8_text(source.as_bytes()) {
                        annotations.push(t.trim().to_string());
                    }
                }
            }
        }
    }
    annotations
}

fn enclosing_method_name(node: Node<'_>, source: &str) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "method_declaration" || n.kind() == "constructor_declaration" {
            return n
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
        }
        cur = n.parent();
    }
    None
}

fn receiver_looks_like_provider(call: Node<'_>, source: &str) -> bool {
    let Some(rx) = call
        .child_by_field_name("object")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
    else {
        return false;
    };
    let rx = rx.trim();
    // Heuristic: receiver is an identifier that ends in `Provider`,
    // `dslProvider`, `dslCtxProvider`, or matches `*Provider` and is followed by `.get()`.
    rx.ends_with("Provider") || rx.ends_with("provider")
}

// ===========================================================================
// Plan kind: java_jooq_projection_mapping_analysis
// ===========================================================================

pub(crate) fn plan_java_jooq_projection_mapping_analysis(p: &RefactorPlanParams) -> Result<String> {
    let per_file = analyze_each_source(p, analyze_projection_mapping_in)?;
    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_projection_mapping_analysis",
        "title": multi_title("jOOQ projection mapping analysis", &per_file),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "files": per_file.iter().map(|(src, findings)| serde_json::json!({
            "source": src,
            "findings": findings,
        })).collect::<Vec<_>>(),
        "caveats": [
            "Arity comparison is textual only; constructor and selected-field \
             counts are best-effort token counts, not validated against types.",
            "Nullability and primitive risk are NOT reported here — this kind \
             surfaces only mapping target shape."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectionFinding {
    line: usize,
    column: usize,
    snippet: String,
    mapping_target: String,
    /// Best-effort comma-count inside the immediately preceding `select(...)`
    /// or `selectFrom(...).select(...)` chain. None when not detectable.
    visible_field_count: Option<usize>,
    /// Whether the mapping target text references a generated record (heuristic).
    references_generated_record: bool,
    /// Whether the surrounding chain text contains `DSL.multiset(` or
    /// `multiset(`.
    nested_multiset: bool,
}

fn analyze_projection_mapping_in(
    _source: &str,
    parsed: &ParsedSource,
) -> Result<Vec<ProjectionFinding>> {
    if parsed.language != "java" {
        bail!("java_jooq_projection_mapping_analysis only supports java files");
    }
    let mut out = Vec::new();
    let root = parsed.tree.root_node();
    walk_projection(root, &parsed.source, &mut out);
    out.sort_by_key(|f| (f.line, f.column));
    Ok(out)
}

fn walk_projection(node: Node<'_>, source: &str, out: &mut Vec<ProjectionFinding>) {
    if node.kind() == "method_invocation" {
        if let Some(name) = method_name_text(node, source) {
            if name == "mapping" && receiver_text_of(node, source).as_deref() == Some("Records") {
                let (line, column) = call_site_line_col(node, source);
                let snippet = snippet_for_node(node, source);
                let target = mapping_first_argument_text(node, source).unwrap_or_default();
                // Best effort: walk up to the nearest containing chain head
                // and inspect for select(...) field counts.
                let chain_text = enclosing_chain_text(node, source);
                let visible_field_count = approximate_select_field_count(&chain_text);
                let references_generated_record =
                    target.contains("Record") || target.contains(".jooq.tables.records");
                let nested_multiset =
                    chain_text.contains("DSL.multiset") || chain_text.contains("multiset(");
                out.push(ProjectionFinding {
                    line,
                    column,
                    snippet,
                    mapping_target: target,
                    visible_field_count,
                    references_generated_record,
                    nested_multiset,
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_projection(child, source, out);
    }
}

fn receiver_text_of(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("object")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(|s| s.trim().to_string())
}

fn mapping_first_argument_text(call: Node<'_>, source: &str) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let arg = args.named_children(&mut cursor).next()?;
    let t = arg.utf8_text(source.as_bytes()).ok()?.trim();
    Some(t.to_string())
}

fn enclosing_chain_text(node: Node<'_>, source: &str) -> String {
    // Walk up until we leave the chain — i.e., until parent is no longer a
    // method_invocation/argument_list.
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "method_invocation" | "argument_list" => cur = parent,
            _ => break,
        }
    }
    source[cur.start_byte()..cur.end_byte()].to_string()
}

fn approximate_select_field_count(chain_text: &str) -> Option<usize> {
    let idx = chain_text
        .find(".select(")
        .or_else(|| chain_text.find("select("))?;
    let body_start = chain_text[idx..].find('(')? + idx + 1;
    let bytes = chain_text.as_bytes();
    let mut depth = 1;
    let mut end = body_start;
    while end < bytes.len() && depth > 0 {
        match bytes[end] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        end += 1;
    }
    if depth != 0 {
        return None;
    }
    let args = &chain_text[body_start..end - 1];
    Some(top_level_comma_count(args) + 1)
}

fn top_level_comma_count(text: &str) -> usize {
    let mut depth = 0;
    let mut count = 0;
    let mut in_string = false;
    let mut prev = '\0';
    for ch in text.chars() {
        if ch == '"' && prev != '\\' {
            in_string = !in_string;
        }
        if !in_string {
            match ch {
                '(' | '<' | '[' | '{' => depth += 1,
                ')' | '>' | ']' | '}' if depth > 0 => depth -= 1,
                ',' if depth == 0 => count += 1,
                _ => {}
            }
        }
        prev = ch;
    }
    count
}

// ===========================================================================
// Plan kind: java_jooq_raw_sql_fragment_audit
// ===========================================================================

pub(crate) fn plan_java_jooq_raw_sql_fragment_audit(p: &RefactorPlanParams) -> Result<String> {
    let per_file = analyze_each_source(p, audit_raw_sql_fragments_in)?;
    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_raw_sql_fragment_audit",
        "title": multi_title("jOOQ raw SQL fragment audit", &per_file),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "files": per_file.iter().map(|(src, findings)| serde_json::json!({
            "source": src,
            "findings": findings,
        })).collect::<Vec<_>>(),
        "caveats": [
            "Classification is by string shape only. Operator review is required \
             before any rewrite.",
            "Strings that look like single identifiers may still be tied to \
             generated tables; mapping to DSL.name(...) or generated references \
             is a SEPARATE plan kind."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawFragmentFinding {
    line: usize,
    column: usize,
    helper: String,
    raw_string: String,
    classification: String,
    snippet: String,
    confidence: String,
}

fn audit_raw_sql_fragments_in(
    _source: &str,
    parsed: &ParsedSource,
) -> Result<Vec<RawFragmentFinding>> {
    if parsed.language != "java" {
        bail!("java_jooq_raw_sql_fragment_audit only supports java files");
    }
    let mut out = Vec::new();
    let root = parsed.tree.root_node();
    walk_raw_fragments(root, &parsed.source, &mut out);
    out.sort_by_key(|f| (f.line, f.column));
    Ok(out)
}

fn walk_raw_fragments(node: Node<'_>, source: &str, out: &mut Vec<RawFragmentFinding>) {
    if node.kind() == "method_invocation" {
        if let Some(name) = method_name_text(node, source) {
            if matches!(name, "field" | "table" | "condition") {
                if matches!(
                    receiver_text_of(node, source).as_deref(),
                    Some("DSL") | Some("org.jooq.impl.DSL"),
                ) {
                    if let Some(arg0) = first_string_argument(node, source) {
                        let classification = classify_fragment(name, &arg0);
                        let (line, column) = call_site_line_col(node, source);
                        out.push(RawFragmentFinding {
                            line,
                            column,
                            helper: format!("DSL.{name}"),
                            raw_string: arg0.clone(),
                            classification: classification.to_string(),
                            snippet: snippet_for_node(node, source),
                            confidence: "medium".to_string(),
                        });
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_raw_fragments(child, source, out);
    }
}

fn first_string_argument(call: Node<'_>, source: &str) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let arg = args.named_children(&mut cursor).next()?;
    if arg.kind() == "string_literal" {
        let raw = arg.utf8_text(source.as_bytes()).ok()?.trim();
        let trimmed = raw.trim_matches('"');
        return Some(trimmed.to_string());
    }
    None
}

fn classify_fragment(helper: &str, raw: &str) -> &'static str {
    let s = raw.trim();
    let has_space = s.contains(' ');
    let looks_dynamic = s.contains('?')
        || s.contains('=')
        || s.to_lowercase().contains(" and ")
        || s.to_lowercase().contains(" or ")
        || s.contains('(');
    let is_dot_separated = s
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(is_sql_ident_char));
    let is_bare_ident = !s.is_empty() && s.chars().all(is_sql_ident_char);

    if helper == "condition" {
        if looks_dynamic {
            return "dynamic_sql";
        }
        return "ambiguous_operator_review";
    }
    if is_bare_ident {
        return "generated_replacement_candidate";
    }
    if is_dot_separated && !has_space {
        return "name_expression";
    }
    if looks_dynamic || has_space {
        return "dynamic_sql";
    }
    "ambiguous_operator_review"
}

fn is_sql_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

// ===========================================================================
// Plan kind: java_jooq_generated_record_boundary_analysis
// ===========================================================================

pub(crate) fn plan_java_jooq_generated_record_boundary_analysis(
    p: &RefactorPlanParams,
) -> Result<String> {
    let per_file = analyze_each_source(p, |src, parsed| {
        analyze_generated_record_boundary_in(src, parsed)
    })?;
    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_jooq_generated_record_boundary_analysis",
        "title": multi_title("jOOQ generated-record boundary analysis", &per_file),
        "semantic_status": SEMANTIC_STATUS,
        "dry_run": true,
        "files": per_file.iter().map(|(src, findings)| serde_json::json!({
            "source": src,
            "findings": findings,
        })).collect::<Vec<_>>(),
        "caveats": [
            "Detection is by import path and `*Record` type-name suffix. \
             Locally-defined non-generated classes whose names happen to end in \
             `Record` may yield false positives; the operator confirms.",
            "Return-type changes are out of scope here; see the matching \
             boundary_rewrite plan kind."
        ],
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "finding_kind", rename_all = "snake_case")]
enum BoundaryFinding {
    GeneratedRecordImport {
        line: usize,
        column: usize,
        import_path: String,
    },
    MethodReturnsGeneratedRecord {
        line: usize,
        column: usize,
        method_name: String,
        return_type: String,
        snippet: String,
    },
    FieldOfGeneratedRecord {
        line: usize,
        column: usize,
        field_name: String,
        type_text: String,
    },
}

fn analyze_generated_record_boundary_in(
    _source: &str,
    parsed: &ParsedSource,
) -> Result<Vec<BoundaryFinding>> {
    if parsed.language != "java" {
        bail!("java_jooq_generated_record_boundary_analysis only supports java files");
    }
    let mut out: Vec<BoundaryFinding> = Vec::new();
    let mut imported_record_types: BTreeSet<String> = BTreeSet::new();
    let root = parsed.tree.root_node();
    collect_imports(root, &parsed.source, &mut out, &mut imported_record_types);
    walk_boundary_members(root, &parsed.source, &imported_record_types, &mut out);
    // Sort by (line, column) for the structured findings; imports already
    // appear in source order.
    out.sort_by_key(|f| match f {
        BoundaryFinding::GeneratedRecordImport { line, column, .. }
        | BoundaryFinding::MethodReturnsGeneratedRecord { line, column, .. }
        | BoundaryFinding::FieldOfGeneratedRecord { line, column, .. } => (*line, *column),
    });
    Ok(out)
}

fn collect_imports(
    root: Node<'_>,
    source: &str,
    out: &mut Vec<BoundaryFinding>,
    short_names: &mut BTreeSet<String>,
) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "import_declaration" {
            if let Ok(text) = child.utf8_text(source.as_bytes()) {
                let import_path = text
                    .trim()
                    .trim_start_matches("import")
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                let looks_generated = import_path.contains(".jooq.tables.records")
                    || (import_path.ends_with("Record") && import_path.contains(".tables."));
                if looks_generated {
                    let (line, column) = line_col(source, child.start_byte());
                    if let Some(short) = import_path.rsplit('.').next() {
                        short_names.insert(short.to_string());
                    }
                    out.push(BoundaryFinding::GeneratedRecordImport {
                        line,
                        column,
                        import_path,
                    });
                }
            }
        }
    }
}

fn walk_boundary_members(
    node: Node<'_>,
    source: &str,
    imported_record_types: &BTreeSet<String>,
    out: &mut Vec<BoundaryFinding>,
) {
    if node.kind() == "method_declaration" {
        if let Some(ret) = node.child_by_field_name("type") {
            if let Ok(ret_text) = ret.utf8_text(source.as_bytes()) {
                let r = ret_text.trim();
                if looks_like_record_type(r, imported_record_types) {
                    let name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                        .unwrap_or("(unnamed)")
                        .to_string();
                    let (line, column) = line_col(source, node.start_byte());
                    out.push(BoundaryFinding::MethodReturnsGeneratedRecord {
                        line,
                        column,
                        method_name: name,
                        return_type: r.to_string(),
                        snippet: first_line_of(source, node.start_byte(), node.end_byte()),
                    });
                }
            }
        }
    }
    if node.kind() == "field_declaration" {
        if let Some(type_node) = node.child_by_field_name("type") {
            if let Ok(t) = type_node.utf8_text(source.as_bytes()) {
                let t = t.trim();
                if looks_like_record_type(t, imported_record_types) {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if child.kind() == "variable_declarator" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                                    let (line, column) = line_col(source, name_node.start_byte());
                                    out.push(BoundaryFinding::FieldOfGeneratedRecord {
                                        line,
                                        column,
                                        field_name: name.to_string(),
                                        type_text: t.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_boundary_members(child, source, imported_record_types, out);
    }
}

fn looks_like_record_type(text: &str, imported: &BTreeSet<String>) -> bool {
    let bare = text.trim().trim_end_matches('?');
    let head = bare.split('<').next().unwrap_or(bare).trim();
    let last_segment = head.rsplit('.').next().unwrap_or(head);
    if head.contains(".jooq.tables.records") {
        return true;
    }
    if !last_segment.ends_with("Record") {
        return false;
    }
    // Avoid `java.lang.Record` and bare `Record` (Java 16 record class):
    if last_segment == "Record" {
        return false;
    }
    imported.contains(last_segment) || head.contains(".tables.records.")
}

fn first_line_of(source: &str, start: usize, end: usize) -> String {
    let raw = &source[start..end.min(source.len())];
    let first = raw.lines().next().unwrap_or(raw).trim();
    if first.len() > 200 {
        format!("{}…", &first[..200])
    } else {
        first.to_string()
    }
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn analyze_each_source<T, F>(
    p: &RefactorPlanParams,
    mut analyze: F,
) -> Result<Vec<(String, Vec<T>)>>
where
    T: Serialize,
    F: FnMut(&str, &ParsedSource) -> Result<Vec<T>>,
{
    let mut out = Vec::new();
    if let Some(sources) = p.sources.as_deref().filter(|s| !s.is_empty()) {
        for src in sources {
            let path = resolve_path(p.project_dir.as_deref(), src)?;
            let parsed = parse_source_file(&path)?;
            let findings = analyze(&path_string(&path), &parsed)?;
            out.push((path_string(&path), findings));
        }
        return Ok(out);
    }
    if p.source.is_empty() {
        bail!("source or sources is required");
    }
    let path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&path)?;
    let findings = analyze(&path_string(&path), &parsed)?;
    out.push((path_string(&path), findings));
    Ok(out)
}

fn multi_title<T>(label: &str, per_file: &[(String, Vec<T>)]) -> String {
    if per_file.len() == 1 {
        format!("{label} on {}", per_file[0].0)
    } else {
        format!("{label} on {} files", per_file.len())
    }
}

fn method_name_text<'a>(call: Node<'_>, source: &'a str) -> Option<&'a str> {
    call.child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()
}

fn call_site_line_col(call: Node<'_>, source: &str) -> (usize, usize) {
    let anchor = call.child_by_field_name("name").unwrap_or(call);
    line_col(source, anchor.start_byte())
}

fn snippet_for_node(node: Node<'_>, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    let raw = &source[start..end];
    let first_line = raw.lines().next().unwrap_or(raw).trim();
    if first_line.len() > 200 {
        format!("{}…", &first_line[..200])
    } else {
        first_line.to_string()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_planner<F>(src: &str, planner: F) -> serde_json::Value
    where
        F: Fn(&RefactorPlanParams) -> Result<String>,
    {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Probe.java");
        fs::write(&file, src).unwrap();
        let params = RefactorPlanParams {
            kind: String::new(),
            source: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let json = planner(&params).expect("planner should succeed");
        serde_json::from_str(&json).unwrap()
    }

    // -- codegen inventory ---------------------------------------------------

    #[test]
    fn codegen_inventory_finds_gradle_jooq_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("build.gradle");
        fs::write(
            &p,
            "// snippet\n\
             jooq {\n\
            \x20   configuration {\n\
            \x20       generator {\n\
            \x20           database { inputSchema = 'PUBLIC' }\n\
            \x20           strategy { name = 'custom' }\n\
            \x20       }\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = RefactorPlanParams {
            kind: String::new(),
            source: String::new(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let json = plan_java_jooq_codegen_config_inventory(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entries = v["entries"].as_array().unwrap();
        assert!(!entries.is_empty(), "expected at least one entry");
        let hints = entries[0]["hints"].as_array().unwrap();
        assert!(
            hints.iter().any(|h| h["token"] == "jooq {"),
            "should record jooq block token"
        );
    }

    #[test]
    fn codegen_inventory_records_pom_forced_type() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pom.xml");
        fs::write(
            &p,
            "<project>\n\
            \x20 <build>\n\
            \x20   <plugins>\n\
            \x20     <plugin><artifactId>jooq-codegen-maven</artifactId>\n\
            \x20       <configuration><generator>\n\
            \x20         <database>\n\
            \x20           <forcedType><name>uuid</name></forcedType>\n\
            \x20         </database>\n\
            \x20       </generator></configuration>\n\
            \x20     </plugin>\n\
            \x20   </plugins>\n\
            \x20 </build>\n\
             </project>\n",
        )
        .unwrap();
        let params = RefactorPlanParams {
            kind: String::new(),
            source: String::new(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let json = plan_java_jooq_codegen_config_inventory(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let hints: Vec<&serde_json::Value> = v["entries"][0]["hints"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        assert!(
            hints.iter().any(|h| h["tag"] == "forcedType"),
            "forcedType tag must be recorded"
        );
    }

    // -- query structure analysis -------------------------------------------

    #[test]
    fn query_structure_finds_select_fetch_chain() {
        let src = "package p;\n\
                   import org.jooq.DSLContext;\n\
                   public class V {\n\
                  \x20   DSLContext dsl;\n\
                  \x20   void run() {\n\
                  \x20       var rows = dsl.select(USER.ID, USER.NAME)\n\
                  \x20                     .from(USER)\n\
                  \x20                     .where(USER.ACTIVE.eq(true))\n\
                  \x20                     .fetch(Records.mapping(UserDto::new));\n\
                  \x20   }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_query_structure_analysis);
        let findings = v["findings"].as_array().unwrap();
        assert!(!findings.is_empty(), "expected at least one chain finding");
        let f = &findings[0]["finding"];
        assert_eq!(f["terminal"], "fetch");
        let methods: Vec<String> = f["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert!(methods.contains(&"select".to_string()));
        assert!(methods.contains(&"fetch".to_string()));
        assert_eq!(f["mapping_target"], "UserDto::new");
        assert_eq!(f["receiver_text"], "dsl");
    }

    #[test]
    fn query_structure_flags_transaction_wrapping() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   org.jooq.DSLContext dsl;\n\
                  \x20   void run() {\n\
                  \x20       dsl.transaction(cfg -> {\n\
                  \x20           cfg.dsl().selectFrom(USER).fetch();\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_query_structure_analysis);
        let findings = v["findings"].as_array().unwrap();
        // The selectFrom().fetch() chain must be tagged as inside transaction.
        let hit = findings
            .iter()
            .find(|f| f["finding"]["terminal"] == "fetch")
            .expect("missing fetch chain");
        assert_eq!(hit["finding"]["transaction_wrapping"], "transaction");
    }

    // -- DSLContext audit ---------------------------------------------------

    #[test]
    fn dsl_context_audit_finds_field_and_provider_and_tx() {
        let src = "package p;\n\
                   import org.jooq.DSLContext;\n\
                   import jakarta.inject.Provider;\n\
                   public class V {\n\
                  \x20   @SchemaA DSLContext dsl;\n\
                  \x20   Provider<DSLContext> dslProvider;\n\
                  \x20   void run() {\n\
                  \x20       dslProvider.get().execute();\n\
                  \x20       dsl.transaction(cfg -> {\n\
                  \x20           cfg.dsl().transactionResult(c2 -> { return null; });\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_dsl_context_audit);
        let findings = v["files"][0]["findings"].as_array().unwrap();
        let by_kind: Vec<&str> = findings
            .iter()
            .map(|f| f["finding_kind"].as_str().unwrap_or(""))
            .collect();
        assert!(by_kind.contains(&"dsl_context_field"));
        assert!(by_kind.contains(&"provider_get"));
        assert!(by_kind.contains(&"transaction_callback"));
        assert!(by_kind.contains(&"nested_transaction"));
        // Provider classification on the field.
        let field = findings
            .iter()
            .find(|f| f["finding_kind"] == "dsl_context_field" && f["variable"] == "dslProvider")
            .expect("missing provider field");
        assert_eq!(field["is_provider"], true);
        // The audit must NEVER claim read/write safety.
        assert!(v["leftovers"].as_array().unwrap().iter().any(|s| {
            s.as_str()
                .unwrap()
                .contains("Read/write safety is NEVER inferred")
        }));
    }

    // -- projection mapping analysis ----------------------------------------

    #[test]
    fn projection_mapping_records_target_and_field_count() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   void run() {\n\
                  \x20       dsl.select(USER.ID, USER.NAME, USER.EMAIL)\n\
                  \x20           .from(USER)\n\
                  \x20           .fetch(Records.mapping(UserDto::new));\n\
                  \x20   }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_projection_mapping_analysis);
        let findings = v["files"][0]["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["mapping_target"], "UserDto::new");
        assert_eq!(findings[0]["visible_field_count"], 3);
        assert_eq!(findings[0]["nested_multiset"], false);
    }

    // -- raw SQL fragment audit ---------------------------------------------

    #[test]
    fn raw_sql_audit_classifies_field_table_condition() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   void run() {\n\
                  \x20       var f = DSL.field(\"my_col\", String.class);\n\
                  \x20       var t = DSL.table(\"public.audit_log\");\n\
                  \x20       var c = DSL.condition(\"x = ? AND y > 0\");\n\
                  \x20       var a = DSL.field(\"some expr + 1\");\n\
                  \x20   }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_raw_sql_fragment_audit);
        let findings = v["files"][0]["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 4);
        let classifications: Vec<&str> = findings
            .iter()
            .map(|f| f["classification"].as_str().unwrap())
            .collect();
        assert!(classifications.contains(&"generated_replacement_candidate"));
        assert!(classifications.contains(&"name_expression"));
        assert!(classifications.contains(&"dynamic_sql"));
    }

    // -- generated record boundary -----------------------------------------

    #[test]
    fn boundary_flags_return_type_and_import() {
        let src = "package p;\n\
                   import com.example.jooq.tables.records.UserRecord;\n\
                   public class V {\n\
                  \x20   UserRecord cached;\n\
                  \x20   public UserRecord findById(long id) { return null; }\n\
                  \x20   public java.lang.Record bogus() { return null; }\n\
                   }\n";
        let v = run_planner(src, plan_java_jooq_generated_record_boundary_analysis);
        let findings = v["files"][0]["findings"].as_array().unwrap();
        let kinds: Vec<&str> = findings
            .iter()
            .map(|f| f["finding_kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"generated_record_import"));
        assert!(kinds.contains(&"method_returns_generated_record"));
        assert!(kinds.contains(&"field_of_generated_record"));
        // `java.lang.Record` must NOT be flagged.
        let bogus = findings.iter().find(|f| f["method_name"] == "bogus");
        assert!(
            bogus.is_none(),
            "java.lang.Record return type must not be flagged"
        );
    }

    // -- non-java rejection across all six --------------------------------

    #[test]
    fn rejects_non_java_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("probe.rs");
        fs::write(&file, "fn main(){}\n").unwrap();
        let params = RefactorPlanParams {
            kind: String::new(),
            source: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        for planner in [
            plan_java_jooq_query_structure_analysis as fn(&RefactorPlanParams) -> Result<String>,
            plan_java_jooq_dsl_context_audit,
            plan_java_jooq_projection_mapping_analysis,
            plan_java_jooq_raw_sql_fragment_audit,
            plan_java_jooq_generated_record_boundary_analysis,
        ] {
            assert!(
                planner(&params).is_err(),
                "non-java source must be rejected"
            );
        }
    }
}
