use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::*;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ObjectSafetyReport {
    #[serde(default)]
    generic_methods: Vec<String>,
    #[serde(default)]
    self_by_value_methods: Vec<String>,
    #[serde(default)]
    associated_constants: Vec<String>,
    #[serde(default)]
    dyn_compatible: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExtractRustTraitPlan {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default)]
    dyn_compatible: bool,
    #[serde(default)]
    object_safety_report: ObjectSafetyReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    call_site_warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trait_in_scope_required: Vec<String>,
}

#[derive(Debug, Clone)]
struct LiftMethod {
    item: SyntaxItem,
    impl_name: String,
    impl_start: usize,
    body_start: usize,
    body_end: usize,
    // kept: parsed method name and full body retained for richer trait-extraction edits
    #[allow(dead_code)]
    method_name: String,
    #[allow(dead_code)]
    signature_with_body: String,
    signature_without_visibility: String,
    method_has_generics: bool,
    method_by_value_self: bool,
    method_is_public: bool,
}

pub fn plan_extract_trait(p: &crate::RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_trait"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let trait_name = p
        .module_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("module_name is required for extract_rust_trait"))?;
    validate_rust_identifier(trait_name, "module_name")?;

    let impl_name = p
        .impl_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("impl_name is required for extract_rust_trait"))?;
    if impl_name.is_empty() {
        bail!("impl_name must not be empty");
    }

    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for extract_rust_trait"))?;

    let parsed = parse_rust_file(&source_path)?;
    let impl_methods = collect_impl_methods(&parsed)?;

    let impl_methods = impl_methods
        .into_iter()
        .filter(|method| method.impl_name == impl_name)
        .collect::<Vec<_>>();
    if impl_methods.is_empty() {
        bail!("no impl block matching `{impl_name}` found");
    }

    let mut selected = Vec::new();
    for expected in names {
        let matches = impl_methods
            .iter()
            .filter(|method| method.item.name.as_deref() == Some(expected.as_str()))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested impl method `{expected}` was not found"),
            [method] => selected.push((*method).clone()),
            _ => bail!(
                "requested impl method `{expected}` matched multiple impl blocks or declarations; pass impl_name"
            ),
        }
    }

    let impl_starts = selected
        .iter()
        .map(|method| method.impl_start)
        .collect::<HashSet<_>>();
    if impl_starts.len() > 1 {
        bail!("extract_rust_trait can only extract methods from one impl block per plan");
    }

    let Some(impl_start) = impl_starts.iter().next().copied() else {
        bail!("extract_rust_trait requires at least one lifted method");
    };

    let source_methods_by_name = impl_methods
        .iter()
        .filter(|method| method.impl_start == impl_start)
        .filter_map(|method| {
            method
                .item
                .name
                .as_ref()
                .map(|name| (name.clone(), method.clone()))
        })
        .collect::<HashMap<_, _>>();

    let selected_names = selected
        .iter()
        .filter_map(|method| method.item.name.clone())
        .collect::<HashSet<_>>();

    let source_impl_target =
        infer_impl_target_type(&source_path, &parsed, &impl_methods, impl_start)?;

    let object_safety = ObjectSafetyReport {
        dyn_compatible: true,
        generic_methods: selected
            .iter()
            .filter(|method| method.method_has_generics)
            .filter_map(|method| method.item.name.clone())
            .collect(),
        self_by_value_methods: selected
            .iter()
            .filter(|method| method.method_by_value_self)
            .filter_map(|method| method.item.name.clone())
            .collect(),
        associated_constants: discover_impl_associated_constants(
            &parsed,
            &source_path,
            &impl_methods,
            impl_start,
        )?,
    };
    let dyn_compatible = object_safety.generic_methods.is_empty()
        && object_safety.self_by_value_methods.is_empty()
        && object_safety.associated_constants.is_empty();

    let selected_ids = selected
        .iter()
        .map(|method| method.item.plan_local_id.clone())
        .collect::<HashSet<_>>();

    let selected_method_names = selected
        .iter()
        .filter_map(|method| method.item.name.as_deref())
        .collect::<Vec<_>>();

    let (call_site_warnings, trait_in_scope_required) = scan_trait_call_sites(
        &selected_method_names,
        &source_impl_target,
        source_path.parent(),
        &p.project_dir,
        &source_path,
    )?;

    for selected_method in &selected {
        let calls = find_self_calls(
            &parsed.source,
            selected_method.body_start,
            selected_method.body_end,
        )?;
        for call in calls {
            if selected_names.contains(&call) {
                continue;
            }
            if let Some(target_method) = source_methods_by_name.get(&call) {
                if target_method.method_is_public {
                    continue;
                }
                bail!(
                    "error.bad_input(code=extract_trait_orphaned_call): lifted method `{}` calls `{}` which is not in the lifted set and is not public",
                    selected_method.item.name.as_deref().unwrap_or("(unnamed)"),
                    call
                );
            }
        }
    }

    let mut selected_by_byte = selected.clone();
    selected_by_byte.sort_by_key(|method| method.item.byte_start);

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let source_edits = selected_by_byte
        .iter()
        .map(|method| TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let target_edit = compose_trait_edit(
        &parsed,
        &target_source,
        trait_name,
        &source_impl_target,
        &selected_by_byte,
    )?;
    let target_edits = vec![TextEdit {
        byte_start: target_source.len(),
        byte_end: target_source.len(),
        replacement: target_edit,
    }];

    let leftovers = impl_methods
        .iter()
        .filter(|method| method.impl_start == impl_start)
        .filter(|method| !selected_ids.contains(&method.item.plan_local_id))
        .map(|method| {
            format!(
                "impl_method {} in {} bytes {}..{}",
                method.item.name.as_deref().unwrap_or("(unnamed)"),
                method.impl_name,
                method.item.byte_start,
                method.item.byte_end
            )
        })
        .collect::<Vec<_>>();

    let plan = ExtractRustTraitPlan {
        plan: RefactorPlan {
            title: format!(
                "extract {} Rust impl method(s) from {} into trait {} in {}",
                selected_by_byte.len(),
                path_string(&source_path),
                trait_name,
                path_string(&target_path)
            ),
            kind: "extract_rust_trait".to_string(),
            semantic_status: SemanticStatus::IndexedHints,
            dry_run: true,
            file_moves: Vec::new(),
            file_creates: Vec::new(),
            edits: vec![
                FileEdit {
                    path: path_string(&source_path),
                    original_sha256: sha256_hex(parsed.source.as_bytes()),
                    edits: source_edits,
                    new_text: None,
                },
                FileEdit {
                    path: path_string(&target_path),
                    original_sha256: sha256_hex(target_source.as_bytes()),
                    edits: target_edits,
                    new_text: None,
                },
            ],
            validations: vec![
                ValidationStep::TreeSitterNoErrors {
                    path: path_string(&source_path),
                    byte_range: None,
                },
                ValidationStep::TreeSitterNoErrors {
                    path: path_string(&target_path),
                    byte_range: None,
                },
            ],
            items: selected_by_byte
                .iter()
                .map(|method| method.item.clone())
                .collect(),
            leftovers,
            captured_variables: Vec::new(),
            remaining_source_accessors: Vec::new(),
            remaining_source_constant_refs: Vec::new(),
            external_calls: Vec::new(),
            inherited_dependencies: Vec::new(),
            deep_analysis: None,
            plan_status: PlanStatus::Planned,
            fixme_count: None,
            operator_opt_outs_used: Vec::new(),
        },
        dyn_compatible,
        object_safety_report: object_safety,
        call_site_warnings,
        trait_in_scope_required,
    };

    validate_plan_shape(&plan.plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn collect_impl_methods(parsed: &ParsedSource) -> Result<Vec<LiftMethod>> {
    let mut methods = Vec::new();
    let mut cursor = parsed.tree.root_node().walk();
    for node in parsed.tree.root_node().named_children(&mut cursor) {
        if node.kind() != "impl_item" {
            continue;
        }
        let Some(impl_name) = item_name(node, &parsed.source, parsed.language) else {
            continue;
        };
        let Some(body) = impl_declaration_list(node) else {
            continue;
        };
        let mut body_cursor = body.walk();
        for fn_node in body
            .named_children(&mut body_cursor)
            .filter(|member| member.kind() == "function_item")
        {
            let Some(item) = method_syntax_item(parsed, fn_node) else {
                continue;
            };
            let Some(body_node) = fn_node.child_by_field_name("body") else {
                continue;
            };
            let body_start = body_node.start_byte();
            let body_end = body_node.end_byte();
            let signature_slice = parsed
                .source
                .get(item.byte_start..body_start)
                .ok_or_else(|| anyhow!("invalid function range for {}", item.plan_local_id))?;
            let fn_keyword = rust_visibility_keyword_byte(&parsed.source, &item)?;
            let signature_start = fn_keyword - item.byte_start;

            let method_signature = signature_slice.trim_end();
            let method_name = item
                .name
                .clone()
                .ok_or_else(|| anyhow!("selected method has no name in {}", item.plan_local_id))?;
            let method_has_generics = signature_is_generic(method_signature, signature_start)?;
            let self_by_value = signature_is_self_by_value(method_signature, signature_start)?;
            let visibility_public = signature_is_public(method_signature, signature_start)?;
            let signature_without_visibility =
                signature_without_visibility(method_signature, signature_start)?;

            methods.push(LiftMethod {
                item,
                impl_name: impl_name.clone(),
                impl_start: node.start_byte(),
                body_start,
                body_end,
                method_name,
                signature_with_body: method_signature.to_string(),
                signature_without_visibility: append_trait_where_self_sized(
                    signature_without_visibility,
                    self_by_value,
                ),
                method_has_generics,
                method_by_value_self: self_by_value,
                method_is_public: visibility_public,
            });
        }
    }

    if methods.is_empty() {
        bail!("no Rust impl methods found");
    }

    methods.sort_by_key(|method| method.item.byte_start);
    Ok(methods)
}

fn method_syntax_item(
    parsed: &ParsedSource,
    function_node: tree_sitter::Node<'_>,
) -> Option<SyntaxItem> {
    Some(syntax_item_with_kind(parsed, function_node, "impl_method"))
}

fn signature_is_generic(signature: &str, signature_start: usize) -> Result<bool> {
    let after_fn = signature
        .get(signature_start..)
        .ok_or_else(|| anyhow!("invalid signature range"))?;
    let open_paren = after_fn
        .find('(')
        .ok_or_else(|| anyhow!("invalid function signature"))?;
    let head = &after_fn[..open_paren];
    Ok(head.contains('<') && head.contains('>'))
}

fn signature_is_self_by_value(signature: &str, signature_start: usize) -> Result<bool> {
    let after_fn = signature
        .get(signature_start..)
        .ok_or_else(|| anyhow!("invalid signature range"))?;
    let open_paren = after_fn
        .find('(')
        .ok_or_else(|| anyhow!("invalid function signature"))?;
    let close_paren = matching_paren(after_fn, open_paren)
        .ok_or_else(|| anyhow!("invalid function signature: unmatched function parameter list"))?;
    let params = after_fn[open_paren + 1..close_paren].trim();
    if params.is_empty() {
        return Ok(false);
    }
    let first = params.split(',').next().map(str::trim).unwrap_or_default();
    if first.is_empty() {
        return Ok(false);
    }
    let starts_with_self = first == "self"
        || first.starts_with("self:")
        || first.starts_with("mut self")
        || first.starts_with("mut self:");
    if !starts_with_self {
        return Ok(false);
    }
    if first.starts_with("&self") || first.starts_with("& mut self") {
        return Ok(false);
    }
    Ok(true)
}

fn signature_is_public(signature: &str, signature_start: usize) -> Result<bool> {
    let prefix = signature
        .get(0..signature_start)
        .ok_or_else(|| anyhow!("invalid signature range"))?;
    let trimmed_prefix = prefix.trim_start();
    Ok(trimmed_prefix.starts_with("pub"))
}

fn signature_without_visibility(signature: &str, signature_start: usize) -> Result<String> {
    let head = signature
        .get(0..signature_start)
        .ok_or_else(|| anyhow!("invalid signature range"))?;
    let mut idx = head.len();
    let bytes = signature.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let visible = signature
        .get(idx..)
        .ok_or_else(|| anyhow!("invalid signature range"))?;
    if !visible.starts_with("pub") {
        return Ok(signature.get(idx..).unwrap_or("").to_string());
    }
    let _after_pub = &visible[3..];
    let mut cursor = 3;
    while cursor < visible.len() && visible.as_bytes()[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor < visible.len() && visible.as_bytes()[cursor] == b'(' {
        if let Some(end) = matching_paren(visible, cursor) {
            cursor = end + 1;
        }
    }
    while cursor < visible.len() && visible.as_bytes()[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    Ok(visible
        .get(cursor..)
        .unwrap_or("")
        .to_string()
        .trim_start()
        .to_string())
}

fn append_trait_where_self_sized(mut signature: String, needs_sized: bool) -> String {
    if !needs_sized {
        return signature;
    }
    if signature.contains("where") {
        if let Ok(where_expr) = Regex::new(r"\bwhere\b") {
            if let Some(m) = where_expr.find(&signature) {
                let before = signature.get(..m.start()).unwrap_or("").trim_end();
                let after = signature.get(m.end()..).unwrap_or("").trim();
                if !after.starts_with("Self: Sized") && !after.contains("Self: Sized") {
                    if after.is_empty() {
                        signature = format!("{} where Self: Sized", before);
                    } else {
                        signature = format!("{} where Self: Sized, {}", before, after);
                    }
                }
            }
        }
    } else {
        signature.push_str(" where Self: Sized");
    }
    signature
}

fn matching_paren(text: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0isize;
    let bytes = text.as_bytes().iter().enumerate().skip(open_pos);
    for (idx, byte) in bytes {
        if *byte == b'(' {
            depth += 1;
        } else if *byte == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

fn find_self_calls(source: &str, body_start: usize, body_end: usize) -> Result<Vec<String>> {
    let body = source
        .get(body_start..body_end)
        .ok_or_else(|| anyhow!("invalid method body range"))?;
    let self_call = Regex::new(r"(?m)\bself\s*\.\s*([A-Za-z_]\w*)\s*(?:<[^()>{}]*>)?\s*\(")?;
    let self_cap = Regex::new(r"(?m)\bSelf\s*::\s*([A-Za-z_]\w*)\s*(?:<[^()>{}]*>)?\s*\(")?;
    let mut calls = Vec::new();
    for capture in self_call.captures_iter(body) {
        if let Some(name) = capture.get(1).map(|m| m.as_str()) {
            calls.push(name.to_string());
        }
    }
    for capture in self_cap.captures_iter(body) {
        if let Some(name) = capture.get(1).map(|m| m.as_str()) {
            calls.push(name.to_string());
        }
    }
    calls.sort();
    calls.dedup();
    Ok(calls)
}

fn infer_impl_target_type(
    path: &Path,
    parsed: &ParsedSource,
    methods: &[LiftMethod],
    impl_start: usize,
) -> Result<String> {
    let target_impl = methods
        .iter()
        .find(|method| method.impl_start == impl_start)
        .ok_or_else(|| anyhow!("failed to locate impl block for target methods"))?;
    let impl_node = method_decl_node_by_start(parsed, target_impl.impl_start).ok_or_else(|| {
        anyhow!(
            "failed to locate impl node at byte {} in {}",
            impl_start,
            path.display()
        )
    })?;
    let body = impl_declaration_list(impl_node)
        .ok_or_else(|| anyhow!("impl block for {} missing declaration list", path.display()))?;
    let mut header = parsed
        .source
        .get(impl_node.start_byte()..body.start_byte())
        .ok_or_else(|| anyhow!("invalid impl header range"))?
        .trim()
        .to_string();
    if !header.starts_with("impl") {
        bail!("impl block in {} does not start with impl", path.display());
    }
    header = header[4..].trim().to_string();
    if header.contains(" for ") {
        bail!("extract_rust_trait requires an inherent impl block");
    }
    if let Some(where_pos) = header.rfind(" where ") {
        header = header[..where_pos].trim().to_string();
    }
    if header.is_empty() {
        bail!(
            "failed to infer struct type from impl header in {}",
            path.display()
        );
    }
    Ok(header)
}

fn method_decl_node_by_start(
    parsed: &ParsedSource,
    byte_start: usize,
) -> Option<tree_sitter::Node<'_>> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|&node| node.kind() == "impl_item" && node.start_byte() == byte_start)
}

fn discover_impl_associated_constants(
    parsed: &ParsedSource,
    path: &Path,
    methods: &[LiftMethod],
    impl_start: usize,
) -> Result<Vec<String>> {
    let impl_node = methods
        .iter()
        .find(|method| method.impl_start == impl_start)
        .and_then(|method| method_decl_node_by_start(parsed, method.impl_start))
        .ok_or_else(|| anyhow!("failed to locate impl node in {}", path.display()))?;

    let body = impl_declaration_list(impl_node)
        .ok_or_else(|| anyhow!("impl block missing declaration list in {}", path.display()))?;
    let mut cursor = body.walk();
    let mut associated_consts = Vec::new();
    for child in body.named_children(&mut cursor) {
        if child.kind().contains("const") && child.kind() != "const_block" {
            if let Some(name) = child.child_by_field_name("name") {
                if let Ok(name_text) = name.utf8_text(parsed.source.as_bytes()) {
                    associated_consts.push(name_text.to_string());
                }
            }
        }
    }
    Ok(associated_consts)
}

fn compose_trait_edit(
    parsed: &ParsedSource,
    target_source: &str,
    trait_name: &str,
    impl_target: &str,
    methods: &[LiftMethod],
) -> Result<String> {
    let mut trait_lines = String::new();
    trait_lines.push_str(&format!("pub trait {trait_name} {{\n"));
    for method in methods {
        trait_lines.push('\n');
        let maybe_attrs = method
            .item
            .leading_trivia_start
            .checked_sub(method.item.byte_start)
            .and_then(|_| {
                parsed
                    .source
                    .get(method.item.leading_trivia_start..method.item.byte_start)
            });
        if let Some(attrs) = maybe_attrs {
            trait_lines.push_str(attrs);
        }
        trait_lines.push_str(method.signature_without_visibility.trim_end());
        trait_lines.push_str(";\n");
    }
    trait_lines.push_str("}\n\n");

    trait_lines.push_str(&format!("impl {trait_name} for {impl_target} {{\n"));
    for method in methods {
        trait_lines.push('\n');
        let body_text = parsed
            .source
            .get(method.item.leading_trivia_start..method.item.byte_end)
            .ok_or_else(|| anyhow!("invalid body range for {}", method.item.plan_local_id))?;
        trait_lines.push_str(body_text);
        if !body_text.ends_with('\n') {
            trait_lines.push('\n');
        }
    }
    trait_lines.push_str("}\n");

    let separator = if target_source.is_empty() || target_source.ends_with("\n\n") {
        ""
    } else if target_source.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    Ok(format!("{separator}{trait_lines}"))
}

fn scan_trait_call_sites(
    selected_methods: &[&str],
    struct_name: &str,
    default_scan_root: Option<&Path>,
    project_dir: &Option<String>,
    source_file: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    let scan_root = project_dir
        .as_ref()
        .and_then(|dir| resolve_path(Some(dir), "").ok())
        .or_else(|| default_scan_root.map(Path::to_path_buf));

    let Some(root) = scan_root else {
        return Ok((Vec::new(), Vec::new()));
    };

    let methods_pattern = selected_methods
        .iter()
        .map(|name| regex::escape(name))
        .collect::<Vec<_>>()
        .join("|");
    if methods_pattern.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let ufcs_re = Regex::new(&format!(
        r"(?m)\b{}\s*::\s*(?:(?:{}))\s*\(",
        regex::escape(struct_name),
        methods_pattern
    ))?;
    let as_re = Regex::new(&format!(
        r"(?m)<\s*{}\s+as\s+[A-Za-z_]\w*\s*>\s*::\s*(?:(?:{}))\s*\(",
        regex::escape(struct_name),
        methods_pattern
    ))?;

    let source_module = module_path_for_file(source_file, Some(&root));

    let mut warnings = BTreeSet::new();
    let mut caller_modules = BTreeSet::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        for cap in ufcs_re.captures_iter(&text) {
            if let Some(m) = cap.get(0) {
                let (line, column) = line_col(&text, m.start());
                warnings.insert(format!(
                    "ufcs:{}:{}:{}:{}",
                    path_string(path),
                    line,
                    column,
                    &m.as_str()[..m.end() - m.start()],
                ));
                if let Some(module_path) = module_path_for_file(path, Some(&root)) {
                    caller_modules.insert(module_path);
                }
            }
        }
        for cap in as_re.captures_iter(&text) {
            if let Some(m) = cap.get(0) {
                let (line, column) = line_col(&text, m.start());
                warnings.insert(format!(
                    "as_trait:{}:{}:{}:{}",
                    path_string(path),
                    line,
                    column,
                    &m.as_str()[..m.end() - m.start()],
                ));
                if let Some(module_path) = module_path_for_file(path, Some(&root)) {
                    caller_modules.insert(module_path);
                }
            }
        }
    }

    let call_site_warnings = warnings.into_iter().collect::<Vec<_>>();

    let mut trait_in_scope_required = caller_modules
        .into_iter()
        .filter(|module| source_module.as_deref() != Some(module.as_str()))
        .collect::<Vec<_>>();

    trait_in_scope_required.sort();
    Ok((call_site_warnings, trait_in_scope_required))
}

fn module_path_for_file(path: &Path, project_dir: Option<&Path>) -> Option<String> {
    let project_dir = project_dir?;
    let rel = path.strip_prefix(project_dir).ok()?;
    let mut parts: Vec<String> = Vec::new();

    let mut components = rel.components();
    if let Some(component) = components.next() {
        let seg = component.as_os_str().to_string_lossy();
        if seg != "src" {
            parts.push(seg.to_string());
        }
    }

    for component in components {
        let part = component.as_os_str().to_string_lossy().to_string();
        if part.ends_with(".rs") {
            let base = part.trim_end_matches(".rs");
            if base != "mod" && base != "lib" && base != "main" {
                parts.push(base.to_string());
            }
        } else {
            parts.push(part);
        }
    }

    if parts.is_empty() {
        return Some("crate".to_string());
    }

    Some(format!("crate::{}", parts.join("::")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn project_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            registered_at: "2026-05-07T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
            aliases: Default::default(),
        }
    }

    #[derive(Debug, Deserialize)]
    struct ParsedTraitPlan {
        dyn_compatible: bool,
        call_site_warnings: Vec<String>,
        trait_in_scope_required: Vec<String>,
        object_safety_report: serde_json::Value,
    }

    #[derive(Debug, Deserialize)]
    struct Wrapper {
        #[serde(default)]
        dyn_compatible: bool,
        #[serde(default)]
        call_site_warnings: Vec<String>,
        #[serde(default)]
        trait_in_scope_required: Vec<String>,
        #[serde(default)]
        object_safety_report: serde_json::Value,
    }

    fn read_plan_fields(plan_text: &str) -> ParsedTraitPlan {
        let value = serde_json::from_str::<Wrapper>(plan_text).unwrap();
        ParsedTraitPlan {
            dyn_compatible: value.dyn_compatible,
            call_site_warnings: value.call_site_warnings,
            trait_in_scope_required: value.trait_in_scope_required,
            object_safety_report: value.object_safety_report,
        }
    }

    #[test]
    fn extract_trait_lifts_two_methods_and_reduces_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main.rs");
        let target = dir.path().join("src/trait_api.rs");
        fs::create_dir_all(source.parent().unwrap()).unwrap();

        fs::write(
            &source,
            "struct Store;\n\nimpl Store {\n    pub fn get(&self, key: usize) -> usize { key }\n\n    pub fn set(&self, key: usize) { key; }\n\n    fn drop_old(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["get".into(), "set".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();

        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str::<serde_json::Value>(&plan_text).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
                cwd: None,
                force_path: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let apply_response: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(apply_response.status, "ok");

        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("fn get"));
        assert!(!source_text.contains("fn set"));
        assert!(source_text.contains("fn drop_old"));

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("pub trait StoreApi"));
        assert!(target_text.contains("impl StoreApi for Store"));
        assert!(target_text.contains("fn get"));
        assert!(target_text.contains("fn set"));
    }

    #[test]
    fn extract_trait_marks_self_by_value_with_sized_and_not_dyn_compatible() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main.rs");
        let target = dir.path().join("src/trait_api.rs");
        write_fixture(
            &source,
            "struct Store;\n\nimpl Store {\n    pub fn consume(self) -> Self { self }\n}\n",
        );

        let plan_text = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["consume".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();

        let parsed = read_plan_fields(&plan_text);
        assert!(!parsed.dyn_compatible);
        assert!(
            parsed.object_safety_report["dyn_compatible"]
                .as_bool()
                .unwrap_or(true)
        );

        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str::<serde_json::Value>(&plan_text).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
                cwd: None,
                force_path: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let apply_response: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(apply_response.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("where Self: Sized"));
    }

    #[test]
    fn extract_trait_refuses_private_non_lifted_self_calls() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main.rs");
        write_fixture(
            &source,
            "struct Store;\n\nimpl Store {\n    fn lift(&self) { self.helper() }\n\n    fn helper(&self) {}\n}\n",
        );

        let err = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&dir.path().join("src/trait_api.rs"))),
            item_names: Some(vec!["lift".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("extract_trait_orphaned_call"));
    }

    #[test]
    fn extract_trait_generics_reflect_in_object_safety() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main.rs");
        write_fixture(
            &source,
            "struct Store;\n\nimpl Store {\n    fn generic<T: Clone>(&self, value: T) -> usize { 1 }\n}\n",
        );

        let plan_text = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&dir.path().join("src/trait_api.rs"))),
            item_names: Some(vec!["generic".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();

        let parsed = read_plan_fields(&plan_text);
        assert!(!parsed.dyn_compatible);
        assert!(
            parsed.object_safety_report["generic_methods"]
                .as_array()
                .is_some_and(|vals| vals.iter().any(|v| v == "generic"))
        );
    }

    #[test]
    fn extract_trait_lists_ufcs_and_trait_path_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main.rs");
        write_fixture(
            &source,
            "struct Store;\n\nimpl Store {\n    pub fn do_work(&self) {}\n}\n\nfn call_sites() {\n    Store::do_work();\n    <Store as SomeTrait>::do_work();\n}\n",
        );

        let plan_text = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&dir.path().join("src/trait_api.rs"))),
            item_names: Some(vec!["do_work".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();

        let parsed = read_plan_fields(&plan_text);
        let warning_blob = parsed.call_site_warnings.join("\n");
        assert!(warning_blob.contains("ufcs"));
        assert!(warning_blob.contains("as_trait"));
    }

    #[test]
    fn extract_trait_lists_trait_in_scope_required_for_distant_callers() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();

        let source = src_dir.join("lib.rs");
        let caller = src_dir.join("remote.rs");
        let target = src_dir.join("trait_api.rs");

        fs::write(
            &source,
            "struct Store;\n\nimpl Store {\n    pub fn do_work(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(&caller, "fn call() {\n    Store::do_work();\n}\n").unwrap();

        let plan_text = plan_extract_trait(&RefactorPlanParams {
            kind: "extract_rust_trait".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["do_work".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Store".into()),
            module_name: Some("StoreApi".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();

        let parsed = read_plan_fields(&plan_text);
        assert!(
            parsed
                .trait_in_scope_required
                .iter()
                .any(|path| path.ends_with("remote"))
        );
    }
}
