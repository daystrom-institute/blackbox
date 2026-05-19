//! Mutating jOOQ-aware Java refactor plan kinds — v1 conservative.
//!
//! These planners back the `java_jooq_*` plan kinds proposed in
//! `design/refactor-tools/java/jooq-refactor-tools-proposal.md`. The v1
//! design intentionally favors small, refusal-heavy rewrites and synthesis
//! that take *operator-supplied shape* through the existing generic
//! [`RefactorPlanParams`] fields (`source`, `target`, `module_name`,
//! `old_text`, `new_text`, `target_prelude`, `fields`, `parameters`,
//! `toml_entries`, `item_names`). Nothing here invents schema qualifiers,
//! read/write policy, soft-delete predicates, authorization, transaction
//! semantics, or public-API changes.
//!
//! All planners emit `SemanticStatus::SyntaxOnly`, `dry_run: true`,
//! sha256'd `FileEdit`s with non-overlapping `TextEdit`s, and a
//! `validations` step when the touched path's language is recognized.

use super::*;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Markers that indicate the selected text is jOOQ-shaped enough for
/// rewrite. Conservative: presence of any one of these strings is the
/// only thing the planner relies on. The v1 design defers SQL semantic
/// equivalence checks to apply-time compilation.
const JOOQ_FRAGMENT_HINTS: &[&str] = &[
    "DSL.field(",
    "DSL.table(",
    "DSL.condition(",
    "DSL.name(",
    "DSL.inline(",
    "DSL.val(",
];

const JOOQ_CHAIN_HINTS: &[&str] = &[
    ".select(",
    ".selectFrom(",
    ".selectDistinct(",
    ".from(",
    ".where(",
    ".join(",
    ".leftJoin(",
    ".fetch(",
    ".fetchInto(",
    ".fetchOne(",
    ".fetchOptional(",
    ".fetchAny(",
    ".fetchSingle(",
    ".execute(",
];

const JOOQ_TRANSACTION_HINTS: &[&str] = &[
    ".transaction(",
    ".transactionResult(",
    ".connectionResult(",
    ".connection(",
];

fn find_text_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() {
        return out;
    }
    while start + needle_bytes.len() <= bytes.len() {
        if &bytes[start..start + needle_bytes.len()] == needle_bytes {
            out.push((start, start + needle_bytes.len()));
            start += needle_bytes.len();
        } else {
            start += 1;
        }
    }
    out
}

fn require_unique_match(source: &str, needle: &str, kind: &str) -> Result<(usize, usize)> {
    let matches = find_text_matches(source, needle);
    match matches.len() {
        0 => bail!(
            "{kind}: old_text not found in source; provide more context so the match is exact"
        ),
        1 => Ok(matches[0]),
        n => bail!("{kind}: old_text matched {n} times; provide more surrounding context"),
    }
}

fn require_java_source(path: &Path, kind: &str) -> Result<ParsedSource> {
    let parsed = parse_source_file(path)?;
    if parsed.language != "java" {
        bail!("{kind} only supports java files (got {})", parsed.language);
    }
    Ok(parsed)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn require_nonempty<'a>(value: Option<&'a str>, kind: &str, field: &str) -> Result<&'a str> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("{kind}: `{field}` is required"))
}

fn toml_str<'a>(
    entries: &'a Option<BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> Option<&'a str> {
    entries.as_ref()?.get(key)?.as_str()
}

fn toml_bool(entries: &Option<BTreeMap<String, serde_json::Value>>, key: &str) -> Option<bool> {
    entries.as_ref()?.get(key)?.as_bool()
}

fn is_valid_simple_java_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn derive_java_package_from_path(target: &Path) -> Option<String> {
    let comps: Vec<String> = target
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    let anchor_idx = comps.iter().rposition(|c| c == "java")?;
    let last_idx = comps.len().saturating_sub(1);
    if anchor_idx + 1 >= last_idx {
        return None;
    }
    let pkg = comps[anchor_idx + 1..last_idx].join(".");
    if pkg.is_empty() { None } else { Some(pkg) }
}

/// Locate the byte just before the closing `}` of the first top-level
/// class declaration's class body. Best-effort textual scan — adequate
/// for the conservative v1 helper-insertion path used by the
/// extract/condition planners. Returns the inferred indent string for
/// nested member declarations.
fn first_class_body_insert_point(source: &str) -> Result<(usize, String)> {
    // Find first `class ` or `record ` followed by `{`.
    let mut search_from = 0usize;
    while let Some(rel) = source[search_from..].find("class ") {
        let class_kw = search_from + rel;
        // Skip occurrences inside line comments by checking previous newline.
        let line_start = source[..class_kw].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_prefix = &source[line_start..class_kw];
        let trimmed = line_prefix.trim_start();
        if trimmed.starts_with("//") {
            search_from = class_kw + "class ".len();
            continue;
        }
        // Find the `{` that opens this class body.
        if let Some(open_rel) = source[class_kw..].find('{') {
            let open_byte = class_kw + open_rel;
            // Walk forward, depth-counting on `{` and `}`, skipping strings.
            let bytes = source.as_bytes();
            let mut depth = 1i32;
            let mut i = open_byte + 1;
            let mut in_str = false;
            let mut esc = false;
            while i < bytes.len() {
                let b = bytes[i];
                if in_str {
                    if esc {
                        esc = false;
                    } else if b == b'\\' {
                        esc = true;
                    } else if b == b'"' {
                        in_str = false;
                    }
                } else {
                    match b {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                let indent = "    ".to_string();
                                return Ok((i, indent));
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            bail!("could not locate matching `}}` for class body");
        }
        search_from = class_kw + "class ".len();
    }
    bail!("no top-level `class ` declaration found in source")
}

fn build_plan_response(
    title: String,
    kind: &str,
    file_edits: Vec<FileEdit>,
    validations: Vec<ValidationStep>,
    leftovers: Vec<String>,
) -> Result<String> {
    let plan = RefactorPlan {
        title,
        kind: kind.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers,
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };
    let mut value = serde_json::to_value(&plan)?;
    value["operator_policy_inputs_used"] =
        serde_json::json!(operator_policy_inputs(&plan.leftovers));
    Ok(serde_json::to_string_pretty(&value)?)
}

fn operator_policy_inputs(leftovers: &[String]) -> Vec<String> {
    let joined = leftovers.join("\n");
    let mut used = Vec::new();
    if joined.contains("acknowledge_public_api_change") {
        used.push("acknowledge_public_api_change".to_string());
    }
    if joined.contains("delete_policy") {
        used.push("delete_policy".to_string());
    }
    if joined.contains("soft_delete_predicate") {
        used.push("soft_delete_predicate".to_string());
    }
    if joined.contains("allow_flatten_nested_tx") {
        used.push("allow_flatten_nested_tx".to_string());
    }
    used.sort();
    used.dedup();
    used
}

// ---------------------------------------------------------------------------
// 1. java_jooq_raw_sql_fragment_rewrite
// ---------------------------------------------------------------------------

const KIND_RAW_SQL_REWRITE: &str = "java_jooq_raw_sql_fragment_rewrite";

pub(crate) fn plan_java_jooq_raw_sql_fragment_rewrite(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_RAW_SQL_REWRITE)?;

    let old_text = require_nonempty(p.old_text.as_deref(), KIND_RAW_SQL_REWRITE, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_RAW_SQL_REWRITE, "new_text")?;

    if !contains_any(old_text, JOOQ_FRAGMENT_HINTS) {
        bail!(
            "{KIND_RAW_SQL_REWRITE}: old_text is not jOOQ-shaped (no DSL.field/table/condition/name/inline/val). \
             Refuse to rewrite arbitrary SQL strings — run the audit and resubmit with the audited fragment."
        );
    }
    // Refuse if new_text reintroduces a raw SQL statement-shaped literal.
    let looks_like_statement = new_text
        .to_ascii_uppercase()
        .split_whitespace()
        .any(|tok| matches!(tok, "SELECT" | "INSERT" | "UPDATE" | "DELETE"));
    if looks_like_statement && new_text.contains('"') {
        bail!(
            "{KIND_RAW_SQL_REWRITE}: new_text appears to embed a raw SQL statement string. \
             v1 refuses statement-shaped raw SQL — produce typed jOOQ expressions or run \
             `java_jooq_extract_query_method` instead."
        );
    }

    let (region_start, region_end) =
        require_unique_match(&parsed.source, old_text, KIND_RAW_SQL_REWRITE)?;

    let edit = TextEdit {
        byte_start: region_start,
        byte_end: region_end,
        replacement: new_text.to_string(),
    };

    let leftovers = vec![
        format!(
            "v1 syntax-only rewrite: compile the touched module to confirm semantic equivalence."
        ),
        "Refused to invent dynamic SQL handling, schema qualifiers, or alias semantics."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "rewrite jOOQ raw-SQL fragment in {}",
            path_string(&source_path)
        ),
        KIND_RAW_SQL_REWRITE,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 2. java_jooq_extract_query_method
// ---------------------------------------------------------------------------

const KIND_EXTRACT_QM: &str = "java_jooq_extract_query_method";

pub(crate) fn plan_java_jooq_extract_query_method(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_EXTRACT_QM)?;

    let old_text = require_nonempty(p.old_text.as_deref(), KIND_EXTRACT_QM, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_EXTRACT_QM, "new_text")?;
    let method_name = require_nonempty(p.module_name.as_deref(), KIND_EXTRACT_QM, "module_name")?;
    if !is_valid_simple_java_identifier(method_name) {
        bail!("{KIND_EXTRACT_QM}: module_name `{method_name}` is not a valid Java identifier");
    }
    let helper_decl = require_nonempty(
        p.target_prelude.as_deref(),
        KIND_EXTRACT_QM,
        "target_prelude (helper method declaration body)",
    )?;
    if !helper_decl.contains(method_name) {
        bail!("{KIND_EXTRACT_QM}: target_prelude does not declare a method named `{method_name}`");
    }

    if !contains_any(old_text, JOOQ_CHAIN_HINTS) {
        bail!(
            "{KIND_EXTRACT_QM}: old_text does not look like a jOOQ fluent chain \
             (no .select/.from/.fetch/.execute marker). Refuse: pick a chain-shaped range."
        );
    }
    if contains_any(old_text, JOOQ_TRANSACTION_HINTS) {
        bail!(
            "{KIND_EXTRACT_QM}: old_text crosses a transaction callback (.transaction*/.connection*). \
             Refuse — extract the inner chain only, or run `java_jooq_transaction_boundary_normalize` first."
        );
    }

    let (region_start, region_end) =
        require_unique_match(&parsed.source, old_text, KIND_EXTRACT_QM)?;

    let (insert_byte, _indent) = first_class_body_insert_point(&parsed.source)?;
    if insert_byte <= region_end {
        bail!("{KIND_EXTRACT_QM}: selected range is not inside the first top-level class body");
    }

    let mut edits = vec![
        TextEdit {
            byte_start: region_start,
            byte_end: region_end,
            replacement: new_text.to_string(),
        },
        TextEdit {
            byte_start: insert_byte,
            byte_end: insert_byte,
            replacement: format!("\n{}\n", helper_decl.trim_end()),
        },
    ];
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let leftovers = vec![
        "v1 syntax-only extract: caller is responsible for ensuring the supplied helper body \
         matches the chain's DSLContext source, fetch shape, and mapping target."
            .to_string(),
        "Refused to infer captured parameters; supply target_prelude with the full helper declaration."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "extract jOOQ query chain into `{method_name}` in {}",
            path_string(&source_path)
        ),
        KIND_EXTRACT_QM,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 3. java_jooq_extract_query_object
// ---------------------------------------------------------------------------

const KIND_EXTRACT_QO: &str = "java_jooq_extract_query_object";

pub(crate) fn plan_java_jooq_extract_query_object(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_EXTRACT_QO)?;

    let target_str = require_nonempty(p.target.as_deref(), KIND_EXTRACT_QO, "target")?;
    let target_path = resolve_path(p.project_dir.as_deref(), target_str)?;
    if target_path == source_path {
        bail!("{KIND_EXTRACT_QO}: target must differ from source");
    }
    if target_path.exists() && !p.merge_existing.unwrap_or(false) {
        bail!(
            "{KIND_EXTRACT_QO}: target `{}` already exists; pass merge_existing=true to overwrite",
            target_path.display()
        );
    }
    let class_name = require_nonempty(p.module_name.as_deref(), KIND_EXTRACT_QO, "module_name")?;
    if !is_valid_simple_java_identifier(class_name) {
        bail!("{KIND_EXTRACT_QO}: module_name `{class_name}` is not a valid Java identifier");
    }
    let target_body = require_nonempty(
        p.target_prelude.as_deref(),
        KIND_EXTRACT_QO,
        "target_prelude (the query-object class body)",
    )?;
    let old_text = require_nonempty(p.old_text.as_deref(), KIND_EXTRACT_QO, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_EXTRACT_QO, "new_text")?;
    if !contains_any(old_text, JOOQ_CHAIN_HINTS) {
        bail!("{KIND_EXTRACT_QO}: old_text does not look like a jOOQ query block; refuse");
    }
    if contains_any(old_text, JOOQ_TRANSACTION_HINTS) {
        bail!("{KIND_EXTRACT_QO}: old_text crosses a transaction callback; refuse");
    }

    let (region_start, region_end) =
        require_unique_match(&parsed.source, old_text, KIND_EXTRACT_QO)?;

    let package = derive_java_package_from_path(&target_path);
    let mut target_text = String::new();
    if let Some(pkg) = package.as_deref() {
        target_text.push_str("package ");
        target_text.push_str(pkg);
        target_text.push_str(";\n\n");
    }
    target_text.push_str(target_body.trim_end());
    target_text.push('\n');

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let source_edit = FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: vec![TextEdit {
            byte_start: region_start,
            byte_end: region_end,
            replacement: new_text.to_string(),
        }],
        new_text: None,
    };
    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_text,
        }],
        new_text: None,
    };

    let leftovers = vec![
        "v1 syntax-only extract-to-query-object: operator must declare DSLContext source/qualifier \
         policy in the supplied class body."
            .to_string(),
        "Refused to invent transaction ownership; the new class should accept DSLContext or \
         Provider<DSLContext> matching the source-side injection style."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "extract jOOQ query block into `{class_name}` at {}",
            path_string(&target_path)
        ),
        KIND_EXTRACT_QO,
        vec![source_edit, target_edit],
        parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 4. java_jooq_extract_repository
// ---------------------------------------------------------------------------

const KIND_EXTRACT_REPO: &str = "java_jooq_extract_repository";

pub(crate) fn plan_java_jooq_extract_repository(p: &RefactorPlanParams) -> Result<String> {
    // v1 conservative: emit a structured Blocked plan — repository extraction
    // requires injection style, DSLContext qualifier policy, and transaction
    // boundary policy that the current operator surface cannot fully express.
    // The plan carries no edits, so it cannot be applied; PlanStatus::Blocked
    // also fails-closed at apply-time. Direct the operator to narrower kinds.
    let _ = require_nonempty(Some(p.source.as_str()), KIND_EXTRACT_REPO, "source")?;

    let block_reason = "block_reason: v1 refuses repository-scale extraction. Required \
        operator policy not representable through current RefactorPlanParams fields: DSLContext \
        qualifier, read/write policy, transaction-boundary policy, and target public API. \
        Prefer `java_jooq_extract_query_object` for cohesive query families, then run \
        `java_jooq_synthesize_repository` against an operator-defined model slice."
        .to_string();

    let title = format!(
        "BLOCKED: {KIND_EXTRACT_REPO} v1 cannot represent operator policy needed for `{}`",
        p.module_name.as_deref().unwrap_or("(unspecified)")
    );

    let plan = RefactorPlan {
        title,
        kind: KIND_EXTRACT_REPO.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: Vec::new(),
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: vec![
            block_reason,
            "no_file_edits_emitted: this plan is structurally Blocked and produces no mutations."
                .to_string(),
            "next_steps: run `java_jooq_extract_query_object` per cohesive query family, then \
             `java_jooq_synthesize_repository` with operator-defined fields/parameters."
                .to_string(),
        ],
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Blocked,
        fixme_count: None,
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ---------------------------------------------------------------------------
// 5. java_jooq_synthesize_repository
// ---------------------------------------------------------------------------

const KIND_SYNTH_REPO: &str = "java_jooq_synthesize_repository";

pub(crate) fn plan_java_jooq_synthesize_repository(p: &RefactorPlanParams) -> Result<String> {
    let project_dir_str =
        require_nonempty(p.project_dir.as_deref(), KIND_SYNTH_REPO, "project_dir")?;
    let project_dir = PathBuf::from(project_dir_str);
    if !project_dir.is_dir() {
        bail!(
            "{KIND_SYNTH_REPO}: project_dir `{}` is not a directory",
            project_dir.display()
        );
    }
    let target_str = require_nonempty(p.target.as_deref(), KIND_SYNTH_REPO, "target")?;
    let target_path = resolve_path(Some(project_dir_str), target_str)?;
    if target_path.exists() && !p.merge_existing.unwrap_or(false) {
        bail!(
            "{KIND_SYNTH_REPO}: target `{}` already exists; pass merge_existing=true to overwrite",
            target_path.display()
        );
    }

    let class_name = require_nonempty(p.module_name.as_deref(), KIND_SYNTH_REPO, "module_name")?;
    if !is_valid_simple_java_identifier(class_name) {
        bail!("{KIND_SYNTH_REPO}: module_name `{class_name}` is not a valid Java identifier");
    }

    // `fields` describes constructor-injected dependencies (e.g.
    // Provider<DSLContext>). At minimum we require an explicit DSLContext
    // accessor; we refuse to invent it.
    let dep_fields = p
        .fields
        .as_deref()
        .filter(|fs| !fs.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{KIND_SYNTH_REPO}: fields (constructor-injected deps incl. DSLContext provider) is required"
            )
        })?;
    let has_dsl_context = dep_fields
        .iter()
        .any(|f| f.type_name.contains("DSLContext"));
    if !has_dsl_context {
        bail!(
            "{KIND_SYNTH_REPO}: fields must include a DSLContext or Provider<DSLContext> dependency"
        );
    }

    // `parameters` describes operator-supplied repository methods as
    // (return-type, method-name) tuples. We synthesize stubbed bodies
    // that the operator must fill in.
    let method_specs = p
        .parameters
        .as_deref()
        .filter(|ps| !ps.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{KIND_SYNTH_REPO}: parameters (method-list as type=return-type/name=method-name) is required"
            )
        })?;

    // Refuse delete-shaped method names without explicit operator policy.
    let delete_policy = toml_str(&p.toml_entries, "delete_policy");
    let soft_delete_predicate = toml_str(&p.toml_entries, "soft_delete_predicate");
    let mut soft_delete_methods: Vec<String> = Vec::new();
    for m in method_specs {
        let lname = m.name.to_ascii_lowercase();
        if lname.contains("delete") || lname.contains("remove") || lname.contains("purge") {
            if delete_policy.is_none() {
                bail!(
                    "{KIND_SYNTH_REPO}: method `{}` requires toml_entries.delete_policy \
                     (one of: hard, soft) — destructive synthesis refuses to infer.",
                    m.name
                );
            }
            if delete_policy == Some("soft") {
                if soft_delete_predicate.is_none() {
                    bail!(
                        "{KIND_SYNTH_REPO}: soft delete on `{}` requires toml_entries.soft_delete_predicate \
                         (the exact column/update clause text) — refuse to invent.",
                        m.name
                    );
                }
                soft_delete_methods.push(m.name.clone());
            }
        }
    }
    let soft_delete_predicate_consumed = !soft_delete_methods.is_empty();

    // Optional read/write policy banner.
    let access_policy = toml_str(&p.toml_entries, "access_policy");

    let package = derive_java_package_from_path(&target_path);
    let body = render_repository_skeleton(
        package.as_deref(),
        class_name,
        dep_fields,
        method_specs,
        access_policy,
        delete_policy,
    );

    let original_target = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let mut leftovers = vec![
        "v1 conservative skeleton: each generated method body is a `// FIXME` stub; \
         operator must write the actual jOOQ query."
            .to_string(),
        "DSLContext qualifier annotations, transaction policy, soft-delete predicates, \
         authorization filters, tenant filters, and auditing fields are operator-owned and \
         not synthesized."
            .to_string(),
    ];
    if let Some(policy) = access_policy {
        leftovers.push(format!(
            "access_policy={policy} recorded in class doc comment only"
        ));
    }
    if let Some(policy) = delete_policy {
        leftovers.push(format!(
            "delete_policy={policy} applied to delete-shaped methods"
        ));
    }
    if soft_delete_predicate_consumed {
        // Operator-policy audit: soft_delete_predicate was consumed for at least
        // one delete-shaped method under delete_policy=soft. Recorded so the
        // durable plan carries operator_policy_inputs_used=soft_delete_predicate.
        if let Some(predicate) = soft_delete_predicate {
            leftovers.push(format!(
                "operator_policy_inputs_used=[soft_delete_predicate]; predicate=`{predicate}` \
                 applied to methods: {}",
                soft_delete_methods.join(", ")
            ));
        }
    }

    build_plan_response(
        format!(
            "synthesize jOOQ repository `{class_name}` at {}",
            path_string(&target_path)
        ),
        KIND_SYNTH_REPO,
        vec![FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(&original_target),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: original_target.len(),
                replacement: body,
            }],
            new_text: None,
        }],
        parse_validation_step_for_path(&target_path),
        leftovers,
    )
}

fn render_repository_skeleton(
    package: Option<&str>,
    class_name: &str,
    deps: &[JavaFieldSpec],
    methods: &[JavaParameterSpec],
    access_policy: Option<&str>,
    delete_policy: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(pkg) = package {
        out.push_str("package ");
        out.push_str(pkg);
        out.push_str(";\n\n");
    }
    out.push_str("// FIXME(jooq-repo-synth): operator must add imports for generated jOOQ\n");
    out.push_str(
        "// tables, DSLContext, Provider<DSLContext> qualifier annotation, and any DTO types.\n\n",
    );
    if let Some(policy) = access_policy {
        out.push_str(&format!(
            "/** access_policy: {policy} (operator-declared) */\n"
        ));
    }
    out.push_str(&format!("public class {class_name} {{\n"));
    for dep in deps {
        out.push_str(&format!(
            "    private final {} {};\n",
            dep.type_name, dep.name
        ));
    }
    if !deps.is_empty() {
        out.push('\n');
    }
    // Constructor.
    out.push_str(&format!("    public {class_name}("));
    let param_list = deps
        .iter()
        .map(|d| format!("{} {}", d.type_name, d.name))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&param_list);
    out.push_str(") {\n");
    for dep in deps {
        out.push_str(&format!("        this.{n} = {n};\n", n = dep.name));
    }
    out.push_str("    }\n\n");

    for m in methods {
        let lname = m.name.to_ascii_lowercase();
        let is_delete =
            lname.contains("delete") || lname.contains("remove") || lname.contains("purge");
        if is_delete {
            if let Some(policy) = delete_policy {
                out.push_str(&format!(
                    "    // FIXME(jooq-repo-synth): {policy} delete — operator must fill in the query body.\n"
                ));
            }
        }
        out.push_str(&format!(
            "    public {ret} {name}() {{\n        // FIXME(jooq-repo-synth): supply jOOQ query body.\n        throw new UnsupportedOperationException(\"{name}: synthesized stub\");\n    }}\n\n",
            ret = m.type_name,
            name = m.name
        ));
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// 6. java_jooq_synthesize_query_method
// ---------------------------------------------------------------------------

const KIND_SYNTH_QM: &str = "java_jooq_synthesize_query_method";

pub(crate) fn plan_java_jooq_synthesize_query_method(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_SYNTH_QM)?;

    let method_name = require_nonempty(p.module_name.as_deref(), KIND_SYNTH_QM, "module_name")?;
    if !is_valid_simple_java_identifier(method_name) {
        bail!("{KIND_SYNTH_QM}: module_name `{method_name}` is not a valid Java identifier");
    }
    let method_decl = require_nonempty(
        p.new_text.as_deref(),
        KIND_SYNTH_QM,
        "new_text (the full method declaration text to insert)",
    )?;
    if !method_decl.contains(method_name) {
        bail!("{KIND_SYNTH_QM}: new_text does not declare a method named `{method_name}`");
    }
    if contains_any(method_decl, JOOQ_TRANSACTION_HINTS) {
        bail!(
            "{KIND_SYNTH_QM}: refuse to synthesize methods that open transaction callbacks. \
             Use `java_jooq_transaction_boundary_normalize` after the method is in place."
        );
    }

    let (insert_byte, _indent) = first_class_body_insert_point(&parsed.source)?;
    let edit = TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: format!("\n{}\n", method_decl.trim_end()),
    };

    let leftovers = vec![
        "v1 syntax-only insert: operator-supplied method body is inserted verbatim.".to_string(),
        "Refused to invent database semantics; the planner does not validate jOOQ shapes \
         beyond rejecting transaction-callback openers."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "synthesize jOOQ query method `{method_name}` in {}",
            path_string(&source_path)
        ),
        KIND_SYNTH_QM,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 7. java_jooq_synthesize_dto_projection
// ---------------------------------------------------------------------------

const KIND_SYNTH_DTO: &str = "java_jooq_synthesize_dto_projection";

pub(crate) fn plan_java_jooq_synthesize_dto_projection(p: &RefactorPlanParams) -> Result<String> {
    let target_str = require_nonempty(p.target.as_deref(), KIND_SYNTH_DTO, "target")?;
    let target_path = resolve_path(p.project_dir.as_deref(), target_str)?;
    if target_path.exists() && !p.merge_existing.unwrap_or(false) {
        bail!(
            "{KIND_SYNTH_DTO}: target `{}` already exists; pass merge_existing=true to overwrite",
            target_path.display()
        );
    }
    let dto_name = require_nonempty(p.module_name.as_deref(), KIND_SYNTH_DTO, "module_name")?;
    if !is_valid_simple_java_identifier(dto_name) {
        bail!("{KIND_SYNTH_DTO}: module_name `{dto_name}` is not a valid Java identifier");
    }
    let components = p
        .fields
        .as_deref()
        .filter(|fs| !fs.is_empty())
        .ok_or_else(|| anyhow!("{KIND_SYNTH_DTO}: fields (DTO record components) is required"))?;

    let package = derive_java_package_from_path(&target_path);
    let mut body = String::new();
    if let Some(pkg) = package.as_deref() {
        body.push_str("package ");
        body.push_str(pkg);
        body.push_str(";\n\n");
    }
    body.push_str(&format!("public record {dto_name}(\n"));
    let component_lines = components
        .iter()
        .map(|f| format!("    {} {}", f.type_name, f.name))
        .collect::<Vec<_>>()
        .join(",\n");
    body.push_str(&component_lines);
    body.push_str("\n) {}\n");

    let original_target = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let mut file_edits = vec![FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target.len(),
            replacement: body,
        }],
        new_text: None,
    }];

    // Optional callsite rewrite — when operator supplies source + old_text +
    // new_text, replace a single Records.mapping(...) or lambda site.
    let src_trim = p.source.trim();
    let old_text_opt = p.old_text.as_deref().filter(|s| !s.is_empty());
    let new_text_opt = p.new_text.as_deref().filter(|s| !s.is_empty());
    if !src_trim.is_empty() {
        if let (Some(old_text), Some(new_text)) = (old_text_opt, new_text_opt) {
            let source_path = resolve_path(p.project_dir.as_deref(), src_trim)?;
            if source_path != target_path {
                let parsed = require_java_source(&source_path, KIND_SYNTH_DTO)?;
                let (rs, re) = require_unique_match(&parsed.source, old_text, KIND_SYNTH_DTO)?;
                file_edits.push(FileEdit {
                    path: path_string(&source_path),
                    original_sha256: sha256_hex(parsed.source.as_bytes()),
                    edits: vec![TextEdit {
                        byte_start: rs,
                        byte_end: re,
                        replacement: new_text.to_string(),
                    }],
                    new_text: None,
                });
            }
        }
    }

    let validations = file_edits
        .iter()
        .flat_map(|fe| parse_validation_step_for_path(Path::new(&fe.path)))
        .collect();

    let leftovers = vec![
        "v1 record-shaped DTO: operator must verify component arity matches the jOOQ \
         projection and that nullability-visible types are correct (e.g., Integer vs int)."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "synthesize jOOQ DTO record `{dto_name}` at {}",
            path_string(&target_path)
        ),
        KIND_SYNTH_DTO,
        file_edits,
        validations,
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 8. java_jooq_condition_builder_extract
// ---------------------------------------------------------------------------

const KIND_COND_BUILDER: &str = "java_jooq_condition_builder_extract";

pub(crate) fn plan_java_jooq_condition_builder_extract(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_COND_BUILDER)?;

    let helper_name = require_nonempty(p.module_name.as_deref(), KIND_COND_BUILDER, "module_name")?;
    if !is_valid_simple_java_identifier(helper_name) {
        bail!("{KIND_COND_BUILDER}: module_name `{helper_name}` is not a valid Java identifier");
    }
    let old_text = require_nonempty(p.old_text.as_deref(), KIND_COND_BUILDER, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_COND_BUILDER, "new_text")?;
    let helper_decl = require_nonempty(
        p.target_prelude.as_deref(),
        KIND_COND_BUILDER,
        "target_prelude (helper method declaration text)",
    )?;
    let _params = p.parameters.as_deref().filter(|ps| !ps.is_empty()).ok_or_else(|| {
        anyhow!(
            "{KIND_COND_BUILDER}: parameters (helper method signature) is required to refuse silent capture"
        )
    })?;

    // Conservative shape check — must look like a Condition expression.
    let cond_markers = [
        "Condition",
        "DSL.condition(",
        ".eq(",
        ".ne(",
        ".and(",
        ".or(",
        ".isNull(",
        ".isNotNull(",
        ".in(",
        ".between(",
    ];
    if !contains_any(old_text, &cond_markers) {
        bail!("{KIND_COND_BUILDER}: old_text does not look like a jOOQ Condition expression");
    }

    let (region_start, region_end) =
        require_unique_match(&parsed.source, old_text, KIND_COND_BUILDER)?;

    let (insert_byte, _indent) = first_class_body_insert_point(&parsed.source)?;
    if insert_byte <= region_end {
        bail!("{KIND_COND_BUILDER}: selected range is not inside the first top-level class body");
    }
    let mut edits = vec![
        TextEdit {
            byte_start: region_start,
            byte_end: region_end,
            replacement: new_text.to_string(),
        },
        TextEdit {
            byte_start: insert_byte,
            byte_end: insert_byte,
            replacement: format!("\n{}\n", helper_decl.trim_end()),
        },
    ];
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let leftovers = vec![
        "v1 syntax-only: operator must guarantee the helper preserves authorization, \
         tenant, and soft-delete predicates exactly. Refuse to merge similar conditions \
         whose differences affect result visibility."
            .to_string(),
    ];

    build_plan_response(
        format!(
            "extract jOOQ Condition into helper `{helper_name}` in {}",
            path_string(&source_path)
        ),
        KIND_COND_BUILDER,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 9. java_jooq_transaction_boundary_normalize
// ---------------------------------------------------------------------------

const KIND_TX_NORMALIZE: &str = "java_jooq_transaction_boundary_normalize";

pub(crate) fn plan_java_jooq_transaction_boundary_normalize(
    p: &RefactorPlanParams,
) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_TX_NORMALIZE)?;

    let old_text = require_nonempty(p.old_text.as_deref(), KIND_TX_NORMALIZE, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_TX_NORMALIZE, "new_text")?;

    if !contains_any(old_text, JOOQ_TRANSACTION_HINTS)
        && !old_text.contains("config.dsl()")
        && !old_text.contains("configuration.dsl()")
    {
        bail!(
            "{KIND_TX_NORMALIZE}: old_text does not look like a jOOQ transaction callback \
             or context-mixing site (.transaction*/.connection*/config.dsl())"
        );
    }

    // Operator must explicitly opt in to flattening nested transactions.
    let allow_flatten = toml_bool(&p.toml_entries, "allow_flatten_nested_tx").unwrap_or(false);
    let old_tx_count = JOOQ_TRANSACTION_HINTS
        .iter()
        .map(|m| old_text.matches(m).count())
        .sum::<usize>();
    let new_tx_count = JOOQ_TRANSACTION_HINTS
        .iter()
        .map(|m| new_text.matches(m).count())
        .sum::<usize>();
    let reduces_tx_openers = new_tx_count < old_tx_count;
    if reduces_tx_openers && !allow_flatten {
        bail!(
            "{KIND_TX_NORMALIZE}: rewrite reduces transaction openers ({old_tx_count} -> {new_tx_count}); \
             pass toml_entries.allow_flatten_nested_tx=true to explicitly approve flattening."
        );
    }
    // Consumed iff the operator opt-in actually permitted a reduction we
    // would otherwise have refused. Setting the flag without an actual
    // reduction is not a consumption — leftovers must not claim it then.
    let allow_flatten_consumed = reduces_tx_openers && allow_flatten;

    let (rs, re) = require_unique_match(&parsed.source, old_text, KIND_TX_NORMALIZE)?;

    let mut leftovers = vec![
        "v1 syntax-only: planner does not verify that nested helper methods accept the \
         callback-owned DSLContext. Operator must thread `config.dsl()` through any helpers \
         the new_text references."
            .to_string(),
    ];
    if allow_flatten_consumed {
        leftovers.push(format!(
            "operator_policy_inputs_used=[allow_flatten_nested_tx=true]; transaction openers \
             reduced {old_tx_count} -> {new_tx_count}"
        ));
    }

    build_plan_response(
        format!(
            "normalize jOOQ transaction boundary in {}",
            path_string(&source_path)
        ),
        KIND_TX_NORMALIZE,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: rs,
                byte_end: re,
                replacement: new_text.to_string(),
            }],
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 10. java_jooq_generated_record_boundary_rewrite
// ---------------------------------------------------------------------------

const KIND_RECORD_BOUNDARY: &str = "java_jooq_generated_record_boundary_rewrite";

pub(crate) fn plan_java_jooq_generated_record_boundary_rewrite(
    p: &RefactorPlanParams,
) -> Result<String> {
    // Public-API change — refuse without explicit operator acknowledgement.
    let ack = toml_bool(&p.toml_entries, "acknowledge_public_api_change").unwrap_or(false);
    if !ack {
        bail!(
            "{KIND_RECORD_BOUNDARY}: refuse — generated-record boundary rewrite changes public \
             return types. Set toml_entries.acknowledge_public_api_change=true to proceed."
        );
    }

    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = require_java_source(&source_path, KIND_RECORD_BOUNDARY)?;
    let old_text = require_nonempty(p.old_text.as_deref(), KIND_RECORD_BOUNDARY, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_RECORD_BOUNDARY, "new_text")?;

    let (rs, re) = require_unique_match(&parsed.source, old_text, KIND_RECORD_BOUNDARY)?;

    let mut leftovers = vec![
        "v1 syntax-only: operator-acknowledged public-API change. Callers must be audited \
         and rewritten in the same refactor batch."
            .to_string(),
        "Refused to invent DTO/mapper code; supply the new return type via new_text or run \
         `java_jooq_synthesize_dto_projection` first."
            .to_string(),
    ];
    leftovers.push("operator_opt_outs_used=[acknowledge_public_api_change]".to_string());

    build_plan_response(
        format!(
            "rewrite generated-record boundary in {}",
            path_string(&source_path)
        ),
        KIND_RECORD_BOUNDARY,
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: rs,
                byte_end: re,
                replacement: new_text.to_string(),
            }],
            new_text: None,
        }],
        parse_validation_step_for_path(&source_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// 11. java_jooq_codegen_extension_apply
// ---------------------------------------------------------------------------

const KIND_CODEGEN_APPLY: &str = "java_jooq_codegen_extension_apply";

pub(crate) fn plan_java_jooq_codegen_extension_apply(p: &RefactorPlanParams) -> Result<String> {
    let target_str = require_nonempty(p.target.as_deref(), KIND_CODEGEN_APPLY, "target")?;
    let target_path = resolve_path(p.project_dir.as_deref(), target_str)?;
    if !target_path.is_file() {
        bail!(
            "{KIND_CODEGEN_APPLY}: target `{}` is not an existing file",
            target_path.display()
        );
    }

    // Only known codegen-config file shapes are accepted. Refuse to edit
    // generated Java sources; refuse unknown extensions.
    let allowed_exts: &[&str] = &["xml", "gradle", "kts", "properties", "toml", "json"];
    let ext = target_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !allowed_exts.contains(&ext) {
        bail!(
            "{KIND_CODEGEN_APPLY}: target extension `.{ext}` is not a recognized codegen-config \
             format. v1 only edits .xml/.gradle/.kts/.properties/.toml/.json. Refuse."
        );
    }
    if target_path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("generated" | "generated-src" | "generated-sources")
        )
    }) {
        bail!("{KIND_CODEGEN_APPLY}: refuse to edit a path under a `generated*` directory");
    }

    let old_text = require_nonempty(p.old_text.as_deref(), KIND_CODEGEN_APPLY, "old_text")?;
    let new_text = require_nonempty(p.new_text.as_deref(), KIND_CODEGEN_APPLY, "new_text")?;

    let source_bytes = fs::read(&target_path)?;
    let source_text = std::str::from_utf8(&source_bytes)
        .map_err(|e| anyhow!("{KIND_CODEGEN_APPLY}: target is not UTF-8: {e}"))?;
    let (rs, re) = require_unique_match(source_text, old_text, KIND_CODEGEN_APPLY)?;

    let leftovers = vec![
        "v1 codegen-config exact-text rewrite. Operator must supply the exact database \
         type pattern, Java type, converter/binding class, target schemas, and validation \
         command — none are inferred."
            .to_string(),
        "Refused to broaden a single usage into a project-wide forced-type rule.".to_string(),
    ];

    build_plan_response(
        format!(
            "apply jOOQ codegen extension to {}",
            path_string(&target_path)
        ),
        KIND_CODEGEN_APPLY,
        vec![FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(&source_bytes),
            edits: vec![TextEdit {
                byte_start: rs,
                byte_end: re,
                replacement: new_text.to_string(),
            }],
            new_text: None,
        }],
        parse_validation_step_for_path(&target_path),
        leftovers,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(root: &Path, rel: &str, body: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn java_file_with_chain(root: &Path) -> PathBuf {
        write_file(
            root,
            "src/main/java/com/example/UserDao.java",
            "package com.example;\n\
             import org.jooq.DSLContext;\n\
             import static com.example.generated.Tables.USERS;\n\
             public class UserDao {\n\
                 private final DSLContext dsl;\n\
                 public UserDao(DSLContext dsl) { this.dsl = dsl; }\n\
                 public java.util.List<String> names() {\n\
                     return dsl.select(USERS.NAME).from(USERS).fetch(USERS.NAME);\n\
                 }\n\
             }\n",
        )
    }

    #[test]
    fn raw_sql_rewrite_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/Q.java",
            "package com.example;\n\
             import org.jooq.DSLContext;\n\
             public class Q {\n\
                 public Object x(DSLContext dsl) {\n\
                     return dsl.select(DSL.field(\"u.name\", String.class)).fetch();\n\
                 }\n\
             }\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_RAW_SQL_REWRITE.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some("DSL.field(\"u.name\", String.class)".to_string()),
            new_text: Some("USERS.NAME".to_string()),
            ..Default::default()
        };
        let response = plan_java_jooq_raw_sql_fragment_rewrite(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.kind, KIND_RAW_SQL_REWRITE);
        assert_eq!(plan.edits.len(), 1);
    }

    #[test]
    fn raw_sql_rewrite_refuses_non_jooq_shape() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/Q.java",
            "package com.example;\npublic class Q { String s = \"select 1\"; }\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_RAW_SQL_REWRITE.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some("\"select 1\"".to_string()),
            new_text: Some("\"select 2\"".to_string()),
            ..Default::default()
        };
        let err = plan_java_jooq_raw_sql_fragment_rewrite(&params)
            .expect_err("should refuse non-jooq fragment");
        assert!(err.to_string().contains("not jOOQ-shaped"), "{err}");
    }

    #[test]
    fn extract_query_method_refuses_transaction_crossing() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/Q.java",
            "package com.example;\npublic class Q {\n\
                 public void run(org.jooq.DSLContext dsl) {\n\
                     dsl.transaction(cfg -> { cfg.dsl().select().from(\"t\").fetch(); });\n\
                 }\n\
             }\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_EXTRACT_QM.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            module_name: Some("loadStuff".to_string()),
            old_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().select().from(\"t\").fetch(); });".to_string(),
            ),
            new_text: Some("loadStuff(dsl);".to_string()),
            target_prelude: Some(
                "private void loadStuff(org.jooq.DSLContext dsl) { /* ... */ }".to_string(),
            ),
            ..Default::default()
        };
        let err = plan_java_jooq_extract_query_method(&params).expect_err("must refuse tx");
        assert!(err.to_string().contains("transaction callback"), "{err}");
    }

    #[test]
    fn extract_query_method_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = java_file_with_chain(dir.path());
        let params = RefactorPlanParams {
            kind: KIND_EXTRACT_QM.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            module_name: Some("loadNames".to_string()),
            old_text: Some(
                "dsl.select(USERS.NAME).from(USERS).fetch(USERS.NAME)".to_string(),
            ),
            new_text: Some("loadNames()".to_string()),
            target_prelude: Some(
                "private java.util.List<String> loadNames() {\n    return dsl.select(USERS.NAME).from(USERS).fetch(USERS.NAME);\n}".to_string(),
            ),
            ..Default::default()
        };
        let resp = plan_java_jooq_extract_query_method(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        assert_eq!(plan.edits.len(), 1);
        assert_eq!(plan.edits[0].edits.len(), 2);
    }

    #[test]
    fn synthesize_repository_refuses_delete_without_policy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("src/main/java/com/example/UserRepo.java");
        let params = RefactorPlanParams {
            kind: KIND_SYNTH_REPO.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserRepo".to_string()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            fields: Some(vec![JavaFieldSpec {
                visibility: None,
                type_name: "DSLContext".to_string(),
                name: "dsl".to_string(),
                final_field: Some(true),
            }]),
            parameters: Some(vec![JavaParameterSpec {
                type_name: "void".to_string(),
                name: "deleteById".to_string(),
            }]),
            ..Default::default()
        };
        let err = plan_java_jooq_synthesize_repository(&params).expect_err("should refuse");
        assert!(err.to_string().contains("delete_policy"), "{err}");
    }

    #[test]
    fn synthesize_repository_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("src/main/java/com/example/UserRepo.java");
        let params = RefactorPlanParams {
            kind: KIND_SYNTH_REPO.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserRepo".to_string()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            fields: Some(vec![JavaFieldSpec {
                visibility: None,
                type_name: "DSLContext".to_string(),
                name: "dsl".to_string(),
                final_field: Some(true),
            }]),
            parameters: Some(vec![
                JavaParameterSpec {
                    type_name: "java.util.List<UserRecord>".to_string(),
                    name: "list".to_string(),
                },
                JavaParameterSpec {
                    type_name: "boolean".to_string(),
                    name: "exists".to_string(),
                },
            ]),
            ..Default::default()
        };
        let resp = plan_java_jooq_synthesize_repository(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        let body = &plan.edits[0].edits[0].replacement;
        assert!(body.contains("public class UserRepo"));
        assert!(body.contains("private final DSLContext dsl;"));
        assert!(body.contains("public java.util.List<UserRecord> list()"));
    }

    #[test]
    fn synthesize_dto_projection_emits_record() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("src/main/java/com/example/UserDto.java");
        let params = RefactorPlanParams {
            kind: KIND_SYNTH_DTO.to_string(),
            source: String::new(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserDto".to_string()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            fields: Some(vec![
                JavaFieldSpec {
                    visibility: None,
                    type_name: "Long".to_string(),
                    name: "id".to_string(),
                    final_field: None,
                },
                JavaFieldSpec {
                    visibility: None,
                    type_name: "String".to_string(),
                    name: "name".to_string(),
                    final_field: None,
                },
            ]),
            ..Default::default()
        };
        let resp = plan_java_jooq_synthesize_dto_projection(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        let body = &plan.edits[0].edits[0].replacement;
        assert!(body.contains("public record UserDto("), "{body}");
        assert!(body.contains("Long id"));
        assert!(body.contains("String name"));
    }

    #[test]
    fn condition_builder_refuses_without_parameters() {
        let dir = tempfile::tempdir().unwrap();
        let src = java_file_with_chain(dir.path());
        let params = RefactorPlanParams {
            kind: KIND_COND_BUILDER.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            module_name: Some("matchUser".to_string()),
            old_text: Some("USERS.NAME".to_string()),
            new_text: Some("matchUser()".to_string()),
            target_prelude: Some(
                "private Condition matchUser() { return USERS.NAME.isNotNull(); }".to_string(),
            ),
            ..Default::default()
        };
        let err = plan_java_jooq_condition_builder_extract(&params)
            .expect_err("should refuse without parameters");
        assert!(err.to_string().contains("parameters"), "{err}");
    }

    #[test]
    fn transaction_normalize_refuses_implicit_flattening() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/T.java",
            "package com.example;\npublic class T {\n\
                 public void run(org.jooq.DSLContext dsl) {\n\
                     dsl.transaction(cfg -> { cfg.dsl().transaction(inner -> { inner.dsl().fetch(\"x\"); }); });\n\
                 }\n\
             }\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_TX_NORMALIZE.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().transaction(inner -> { inner.dsl().fetch(\"x\"); }); });".to_string(),
            ),
            new_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().fetch(\"x\"); });".to_string(),
            ),
            ..Default::default()
        };
        let err = plan_java_jooq_transaction_boundary_normalize(&params)
            .expect_err("should refuse flattening without opt-in");
        assert!(err.to_string().contains("allow_flatten_nested_tx"), "{err}");
    }

    #[test]
    fn generated_record_boundary_refuses_without_ack() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/Api.java",
            "package com.example;\npublic class Api {\n\
                 public UserRecord get() { return null; }\n\
             }\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_RECORD_BOUNDARY.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some("public UserRecord get()".to_string()),
            new_text: Some("public UserDto get()".to_string()),
            ..Default::default()
        };
        let err = plan_java_jooq_generated_record_boundary_rewrite(&params)
            .expect_err("should refuse without ack");
        assert!(
            err.to_string().contains("acknowledge_public_api_change"),
            "{err}"
        );
    }

    #[test]
    fn codegen_extension_apply_refuses_java_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(dir.path(), "src/main/java/Generated.java", "package x;\n");
        let params = RefactorPlanParams {
            kind: KIND_CODEGEN_APPLY.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some("x".to_string()),
            new_text: Some("y".to_string()),
            ..Default::default()
        };
        let err = plan_java_jooq_codegen_extension_apply(&params)
            .expect_err("should refuse .java target");
        assert!(
            err.to_string().contains("recognized codegen-config"),
            "{err}"
        );
    }

    #[test]
    fn codegen_extension_apply_happy_xml() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(
            dir.path(),
            "jooq-config.xml",
            "<configuration>\n  <forcedTypes></forcedTypes>\n</configuration>\n",
        );
        let params = RefactorPlanParams {
            kind: KIND_CODEGEN_APPLY.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some("<forcedTypes></forcedTypes>".to_string()),
            new_text: Some(
                "<forcedTypes><forcedType><name>JSONB</name><userType>com.example.Json</userType><converter>com.example.JsonConverter</converter><includeTypes>jsonb</includeTypes></forcedType></forcedTypes>".to_string(),
            ),
            ..Default::default()
        };
        let resp = plan_java_jooq_codegen_extension_apply(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        assert_eq!(plan.edits.len(), 1);
    }

    #[test]
    fn extract_repository_emits_blocked_plan_with_no_edits() {
        let dir = tempfile::tempdir().unwrap();
        let src = java_file_with_chain(dir.path());
        let params = RefactorPlanParams {
            kind: KIND_EXTRACT_REPO.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            module_name: Some("UserRepo".to_string()),
            ..Default::default()
        };
        let resp = plan_java_jooq_extract_repository(&params)
            .expect("extract_repository returns a structured Blocked plan, not Err");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        assert_eq!(plan.kind, KIND_EXTRACT_REPO);
        assert_eq!(plan.semantic_status, SemanticStatus::SyntaxOnly);
        assert!(plan.dry_run);
        assert_eq!(plan.plan_status, PlanStatus::Blocked);
        assert!(plan.edits.is_empty(), "Blocked plan must emit no file edits");
        assert!(
            plan.file_moves.is_empty(),
            "Blocked plan must emit no file moves"
        );
        assert!(
            plan.leftovers.iter().any(|l| l.contains("block_reason")),
            "leftovers must include a block_reason: {:?}",
            plan.leftovers
        );
    }

    #[test]
    fn synthesize_repository_records_soft_delete_predicate_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/UserRepo.java");
        let mut toml_entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        toml_entries.insert(
            "delete_policy".to_string(),
            serde_json::Value::String("soft".to_string()),
        );
        toml_entries.insert(
            "soft_delete_predicate".to_string(),
            serde_json::Value::String(
                "USERS.DELETED_AT.isNull() / set DELETED_AT = currentInstant()".to_string(),
            ),
        );
        let params = RefactorPlanParams {
            kind: KIND_SYNTH_REPO.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserRepo".to_string()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            fields: Some(vec![JavaFieldSpec {
                visibility: None,
                type_name: "DSLContext".to_string(),
                name: "dsl".to_string(),
                final_field: Some(true),
            }]),
            parameters: Some(vec![JavaParameterSpec {
                type_name: "int".to_string(),
                name: "softDeleteById".to_string(),
            }]),
            toml_entries: Some(toml_entries),
            ..Default::default()
        };
        let resp = plan_java_jooq_synthesize_repository(&params)
            .expect("soft-delete with predicate must succeed");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        let policy_line = plan
            .leftovers
            .iter()
            .find(|l| l.contains("operator_policy_inputs_used"))
            .unwrap_or_else(|| {
                panic!(
                    "leftovers must record operator_policy_inputs_used=soft_delete_predicate: {:?}",
                    plan.leftovers
                )
            });
        assert!(
            policy_line.contains("soft_delete_predicate"),
            "leftovers must name soft_delete_predicate, got: {policy_line}"
        );
        assert!(
            policy_line.contains("softDeleteById"),
            "leftovers must list the consuming method, got: {policy_line}"
        );
    }

    #[test]
    fn synthesize_repository_does_not_record_predicate_when_not_consumed() {
        // delete_policy=hard never reads soft_delete_predicate even if supplied;
        // the audit line must not appear.
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/UserRepo.java");
        let mut toml_entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        toml_entries.insert(
            "delete_policy".to_string(),
            serde_json::Value::String("hard".to_string()),
        );
        toml_entries.insert(
            "soft_delete_predicate".to_string(),
            serde_json::Value::String("never used".to_string()),
        );
        let params = RefactorPlanParams {
            kind: KIND_SYNTH_REPO.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserRepo".to_string()),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            fields: Some(vec![JavaFieldSpec {
                visibility: None,
                type_name: "DSLContext".to_string(),
                name: "dsl".to_string(),
                final_field: Some(true),
            }]),
            parameters: Some(vec![JavaParameterSpec {
                type_name: "int".to_string(),
                name: "deleteById".to_string(),
            }]),
            toml_entries: Some(toml_entries),
            ..Default::default()
        };
        let resp = plan_java_jooq_synthesize_repository(&params)
            .expect("hard-delete with explicit policy must succeed");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        assert!(
            !plan
                .leftovers
                .iter()
                .any(|l| l.contains("operator_policy_inputs_used")
                    && l.contains("soft_delete_predicate")),
            "must not record soft_delete_predicate consumption when delete_policy=hard: {:?}",
            plan.leftovers
        );
    }

    #[test]
    fn transaction_normalize_records_flatten_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/T.java",
            "package com.example;\npublic class T {\n\
                 public void run(org.jooq.DSLContext dsl) {\n\
                     dsl.transaction(cfg -> { cfg.dsl().transaction(inner -> { inner.dsl().fetch(\"x\"); }); });\n\
                 }\n\
             }\n",
        );
        let mut toml_entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        toml_entries.insert(
            "allow_flatten_nested_tx".to_string(),
            serde_json::Value::Bool(true),
        );
        let params = RefactorPlanParams {
            kind: KIND_TX_NORMALIZE.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().transaction(inner -> { inner.dsl().fetch(\"x\"); }); });".to_string(),
            ),
            new_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().fetch(\"x\"); });".to_string(),
            ),
            toml_entries: Some(toml_entries),
            ..Default::default()
        };
        let resp = plan_java_jooq_transaction_boundary_normalize(&params)
            .expect("flatten with opt-in must succeed");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        let line = plan
            .leftovers
            .iter()
            .find(|l| l.contains("operator_policy_inputs_used"))
            .unwrap_or_else(|| {
                panic!(
                    "leftovers must record operator_policy_inputs_used=allow_flatten_nested_tx: {:?}",
                    plan.leftovers
                )
            });
        assert!(
            line.contains("allow_flatten_nested_tx=true"),
            "leftovers must name allow_flatten_nested_tx=true, got: {line}"
        );
    }

    #[test]
    fn transaction_normalize_does_not_record_flatten_when_not_reducing() {
        // Opt-in flag set but the rewrite preserves transaction openers — the
        // flag is not actually consumed and must not appear in leftovers.
        let dir = tempfile::tempdir().unwrap();
        let src = write_file(
            dir.path(),
            "src/main/java/com/example/T.java",
            "package com.example;\npublic class T {\n\
                 public void run(org.jooq.DSLContext dsl) {\n\
                     dsl.transaction(cfg -> { cfg.dsl().fetch(\"x\"); });\n\
                 }\n\
             }\n",
        );
        let mut toml_entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        toml_entries.insert(
            "allow_flatten_nested_tx".to_string(),
            serde_json::Value::Bool(true),
        );
        let params = RefactorPlanParams {
            kind: KIND_TX_NORMALIZE.to_string(),
            source: src.to_string_lossy().into_owned(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            old_text: Some(
                "dsl.transaction(cfg -> { cfg.dsl().fetch(\"x\"); });".to_string(),
            ),
            new_text: Some(
                "dsl.transactionResult(cfg -> { return cfg.dsl().fetch(\"x\"); });".to_string(),
            ),
            toml_entries: Some(toml_entries),
            ..Default::default()
        };
        let resp = plan_java_jooq_transaction_boundary_normalize(&params)
            .expect("non-reducing rewrite must succeed");
        let plan: RefactorPlan = serde_json::from_str(&resp).unwrap();
        assert!(
            !plan
                .leftovers
                .iter()
                .any(|l| l.contains("operator_policy_inputs_used")),
            "must not record flatten consumption when rewrite did not reduce openers: {:?}",
            plan.leftovers
        );
    }
}
