//! Read-only "facts" surface for harness-native bindings.
//!
//! The code-mode cell DSL (design/bro-harness/refactor-tools-v2.md §3.1)
//! projects fact bindings like `code.items` / `code.query` into the V8 cell.
//! Those bindings live in `bro-harness` and call into this module — the same
//! tree-sitter machinery the v1 plan kinds use, exposed as plain functions
//! returning data instead of MCP-shaped JSON strings. Pure functions of the
//! file bytes: no LSP, no daemon state, no writes (the harness-native
//! invariant — decision af3c4783 — depends on this module staying that way).

use std::path::Path;

use anyhow::{Result, anyhow};
use std::collections::{BTreeMap, BTreeSet};

use crate::chunker;
use crate::{SyntaxItem, sha256_hex};

/// One inventoried item plus the facts the inventory walk doesn't carry.
///
/// `visibility` exists because its absence actively misleads: probe
/// `probe-code-facts-2` read an empty `attributes` array on a `pub fn` as
/// "this surface doesn't understand visibility" and abandoned the namespace.
#[derive(Debug, Clone)]
pub struct ItemFact {
    pub item: SyntaxItem,
    /// Visibility modifier text (`pub`, `pub(crate)`, `public`, …);
    /// `None` = private/default visibility (or not derivable for the
    /// language).
    pub visibility: Option<String>,
    /// Nearest declaring type name, when derivable.
    pub declaring_type: Option<String>,
    /// True when the item belongs to a nested type.
    pub nested: bool,
}

/// Top-level syntax-item inventory of one source file — the same per-language
/// item walk `bbox_refactor_status` uses, with the source hash captured at
/// read time so callers can mint drift-guarded spans.
#[derive(Debug, Clone)]
pub struct FileItemsFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub items: Vec<ItemFact>,
}

/// Inventory the top-level syntax items of `path`.
pub fn file_items(path: &Path) -> Result<FileItemsFacts> {
    let parsed = super::parse_source_file(path)?;
    let items = match parsed.language {
        "rust" => super::rust_status_items(&parsed),
        "java" => super::java_status_items(&parsed),
        _ => super::generic_top_level_items(&parsed),
    };
    let root = parsed.tree.root_node();
    let items = items
        .into_iter()
        .map(|item| {
            let node = root.named_descendant_for_byte_range(item.byte_start, item.byte_end);
            let visibility =
                node.and_then(|node| item_visibility(node, parsed.language, &parsed.source));
            let (declaring_type, nested) = node
                .and_then(|node| item_declaring_type(node, parsed.language, &parsed.source))
                .map(|type_path| {
                    let nested = type_path.len() > 1;
                    (type_path.last().cloned(), nested)
                })
                .unwrap_or((None, false));
            ItemFact {
                item,
                visibility,
                declaring_type,
                nested,
            }
        })
        .collect();
    Ok(FileItemsFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        source_len: parsed.source.len(),
        items,
    })
}

fn item_declaring_type(
    node: tree_sitter::Node<'_>,
    language: &str,
    source: &str,
) -> Option<Vec<String>> {
    match language {
        "java" => java_type_path(node, source),
        _ => None,
    }
}

fn java_type_path(mut node: tree_sitter::Node<'_>, source: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    loop {
        if java_type_declaration_kind(node.kind())
            && let Some(name) = java_type_name(node, source)
        {
            names.push(name);
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    if names.is_empty() {
        None
    } else {
        names.reverse();
        Some(names)
    }
}

fn java_type_declaration_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    )
}

fn java_type_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| source.get(name.start_byte()..name.end_byte()))
        .map(str::to_string)
}

/// Visibility modifier text of an item node, per language. Rust reads the
/// `visibility_modifier` child; Java reads `public`/`protected`/`private`
/// out of the `modifiers` child. Other languages return `None`.
fn item_visibility(node: tree_sitter::Node<'_>, language: &str, source: &str) -> Option<String> {
    let text_of = |n: tree_sitter::Node<'_>| source.get(n.start_byte()..n.end_byte());
    let mut cursor = node.walk();
    match language {
        "rust" => node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "visibility_modifier")
            .and_then(text_of)
            .map(str::to_string),
        "java" => node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "modifiers")
            .and_then(text_of)
            .and_then(|modifiers| {
                ["public", "protected", "private"]
                    .iter()
                    .find(|keyword| modifiers.split_whitespace().any(|m| m == **keyword))
                    .map(|keyword| keyword.to_string())
            }),
        _ => None,
    }
}

/// One capture from a tree-sitter query run.
#[derive(Debug, Clone)]
pub struct QueryCaptureFact {
    /// Capture name from the query (without `@`).
    pub capture: String,
    /// Node kind of the captured node.
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
    /// Source text of the captured node.
    pub text: String,
}

/// Result of running a tree-sitter query over one file.
#[derive(Debug, Clone)]
pub struct FileQueryFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub captures: Vec<QueryCaptureFact>,
}

/// Hard ceiling on captures returned by one query run, so a pathological
/// query (e.g. `(identifier) @x` over a generated file) stays bounded.
pub const MAX_QUERY_CAPTURES: usize = 5_000;

/// Aggregate ceiling on captures returned by ONE multi-file `code.query`
/// fan-out. The per-file [`MAX_QUERY_CAPTURES`] does not bound a batch over a
/// whole repo: a broad query across a large repo (~1,700 files, probe-dash-1)
/// flattened to a multi-MB JSON array that OOM'd the V8 isolate parsing the
/// tool result. Values-not-refs (cell-dsl §2) means big values live in the
/// isolate heap, so the binding must cap the payload it hands back — the
/// caller narrows the query or the file set and re-runs.
pub const MAX_AGGREGATE_QUERY_CAPTURES: usize = 20_000;

/// Run a tree-sitter query over `path`, optionally restricted to matches
/// intersecting the `within` byte range.
pub fn file_query(
    path: &Path,
    query_src: &str,
    within: Option<(usize, usize)>,
) -> Result<FileQueryFacts> {
    let parsed = super::parse_source_file(path)?;
    let ts_language = chunker::code::ts_language_for_name(parsed.language)?;
    let query = tree_sitter::Query::new(&ts_language, query_src).map_err(|e| {
        anyhow!(
            "invalid tree-sitter query for language {}: {e}",
            parsed.language
        )
    })?;
    let capture_names = query.capture_names();

    let mut cursor = tree_sitter::QueryCursor::new();
    if let Some((start, end)) = within {
        if start > end || end > parsed.source.len() {
            return Err(anyhow!(
                "within range {start}..{end} is out of bounds for {} ({} bytes)",
                path.display(),
                parsed.source.len()
            ));
        }
        cursor.set_byte_range(start..end);
    }

    let mut captures = Vec::new();
    let mut matches = cursor.matches(&query, parsed.tree.root_node(), parsed.source.as_bytes());
    'outer: while let Some(found) = streaming_iterator::StreamingIterator::next(&mut matches) {
        for capture in found.captures {
            if captures.len() >= MAX_QUERY_CAPTURES {
                break 'outer;
            }
            let node = capture.node;
            captures.push(QueryCaptureFact {
                capture: capture_names
                    .get(capture.index as usize)
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
                kind: node.kind().to_string(),
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
                text: parsed
                    .source
                    .get(node.start_byte()..node.end_byte())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }

    Ok(FileQueryFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        captures,
    })
}

/// One Java field declaration fact.
#[derive(Debug, Clone)]
pub struct JavaFieldFact {
    pub name: String,
    pub type_text: String,
    pub owner_class: Option<String>,
    pub visibility: Option<String>,
    pub modifiers: Vec<String>,
    pub annotations: Vec<String>,
    pub is_static: bool,
    pub is_final: bool,
    pub byte_start: usize,
    pub byte_end: usize,
    pub name_byte_start: usize,
    pub name_byte_end: usize,
}

/// Java field declaration inventory for one file.
#[derive(Debug, Clone)]
pub struct FileJavaFieldsFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub fields: Vec<JavaFieldFact>,
}

/// One classified Java field access site.
#[derive(Debug, Clone)]
pub struct JavaFieldAccessFact {
    pub method: Option<String>,
    pub kind: String,
    pub line: usize,
    pub column: usize,
    pub context: String,
}

/// Pre-extract field classification for one Java field.
#[derive(Debug, Clone)]
pub struct JavaFieldClassificationFact {
    pub name: String,
    pub type_text: String,
    pub owner_class: Option<String>,
    pub visibility: Option<String>,
    pub modifiers: Vec<String>,
    pub annotations: Vec<String>,
    pub is_static_final: bool,
    pub is_mutable_instance: bool,
    pub is_injected: bool,
    pub injection_style: Option<String>,
    pub is_provider: bool,
    pub reads: usize,
    pub writes: usize,
    pub read_by: Vec<String>,
    pub written_by: Vec<String>,
    pub accesses: Vec<JavaFieldAccessFact>,
}

/// Field classification payload for one Java file.
#[derive(Debug, Clone)]
pub struct FileJavaFieldClassificationFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub fields: Vec<JavaFieldClassificationFact>,
}

/// Inventory Java fields in `path`, optionally restricted to one owner class.
pub fn java_fields(path: &Path, class_name: Option<&str>) -> Result<FileJavaFieldsFacts> {
    let parsed = super::parse_source_file(path)?;
    if parsed.language != "java" {
        return Err(anyhow!("code.fields only supports java files"));
    }
    let content_sha256 = sha256_hex(parsed.source.as_bytes());
    let source_len = parsed.source.len();
    let fields = java_field_facts_for_parsed(&parsed, class_name);
    Ok(FileJavaFieldsFacts {
        language: parsed.language,
        content_sha256,
        source_len,
        fields,
    })
}

/// Classify Java fields before an extraction, optionally restricted by field
/// names and/or owner class.
pub fn java_field_classification(
    path: &Path,
    field_names: Option<&[String]>,
    class_name: Option<&str>,
) -> Result<FileJavaFieldClassificationFacts> {
    let parsed = super::parse_source_file(path)?;
    if parsed.language != "java" {
        return Err(anyhow!(
            "analysis.fieldClassification only supports java files"
        ));
    }
    let all_fields = java_field_facts_for_parsed(&parsed, class_name);
    let constructor_injected_fields = java_constructor_injected_fields(&parsed, class_name);
    let requested: BTreeSet<&str> = field_names
        .map(|names| names.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let selected: Vec<JavaFieldFact> = all_fields
        .into_iter()
        .filter(|field| requested.is_empty() || requested.contains(field.name.as_str()))
        .collect();
    let mut by_name: BTreeMap<String, Vec<JavaFieldAccessFact>> = selected
        .iter()
        .map(|field| (field.name.clone(), Vec::new()))
        .collect();
    let selected_names: BTreeSet<&str> = selected.iter().map(|field| field.name.as_str()).collect();

    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" {
            continue;
        }
        let Ok(name) = node.utf8_text(parsed.source.as_bytes()) else {
            continue;
        };
        if !selected_names.contains(name) {
            continue;
        }
        let Some(access_node) = java_source_field_access_node(node) else {
            continue;
        };
        if java_identifier_shadowed(node, name, &parsed.source) {
            continue;
        }
        let (line, column) = source_line_col(&parsed.source, access_node.start_byte());
        let method = java_enclosing_callable_name(access_node, &parsed.source);
        let kind = java_access_kind(access_node).to_string();
        let context = source_line_context(&parsed.source, access_node.start_byte());
        if let Some(accesses) = by_name.get_mut(name) {
            accesses.push(JavaFieldAccessFact {
                method,
                kind,
                line,
                column,
                context,
            });
        }
    }

    let mut fields = Vec::new();
    for field in selected {
        let mut accesses = by_name.remove(&field.name).unwrap_or_default();
        accesses.sort_by(|a, b| {
            a.line
                .cmp(&b.line)
                .then(a.column.cmp(&b.column))
                .then(a.kind.cmp(&b.kind))
        });
        let mut read_by = BTreeSet::new();
        let mut written_by = BTreeSet::new();
        let mut reads = 0usize;
        let mut writes = 0usize;
        for access in &accesses {
            let method = access
                .method
                .clone()
                .unwrap_or_else(|| "(class-initializer)".to_string());
            if access.kind == "write" {
                writes += 1;
                written_by.insert(method);
            } else {
                reads += 1;
                read_by.insert(method);
            }
        }
        let is_static_final = field.is_static && field.is_final;
        let field_annotation_injected = field
            .annotations
            .iter()
            .any(|annotation| java_annotation_is_inject(annotation));
        let constructor_injected =
            constructor_injected_fields.contains(&(field.owner_class.clone(), field.name.clone()));
        let injection_style = if field_annotation_injected {
            Some("field_annotation".to_string())
        } else if constructor_injected {
            Some("constructor_param".to_string())
        } else {
            None
        };
        let is_provider =
            field.type_text.contains("Provider<") || field.type_text.ends_with("Provider");
        fields.push(JavaFieldClassificationFact {
            name: field.name,
            type_text: field.type_text,
            owner_class: field.owner_class,
            visibility: field.visibility,
            modifiers: field.modifiers,
            annotations: field.annotations,
            is_static_final,
            is_mutable_instance: !is_static_final && !field.is_final && !field.is_static,
            is_injected: injection_style.is_some(),
            injection_style,
            is_provider,
            reads,
            writes,
            read_by: read_by.into_iter().collect(),
            written_by: written_by.into_iter().collect(),
            accesses,
        });
    }

    Ok(FileJavaFieldClassificationFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        source_len: parsed.source.len(),
        fields,
    })
}

/// Transitive field/constant initializer dependency closure for one Java file.
///
/// For each field in `field_names` that is static final, this walks the field's
/// initializer and finds references to OTHER static final fields/constants in
/// the same file. It then follows those references transitively to produce the
/// full closure — the set of all source constants that must move with each
/// requested field to preserve compile-time validity.
///
/// This prevents the "moved `CONDENSATE_ADDITIONAL_COLUMNS` but its initializer
/// referenced `ADJUSTED_TOTAL` which stayed in the source" class of
/// extract-time compile failure.
///
/// Returns `{ field_name: [transitive_dep_name, ...] }` — only fields that
/// actually have transitive dependencies are included.
pub fn java_field_initializer_closure(
    path: &Path,
    field_names: &[String],
    class_name: Option<&str>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let parsed = super::parse_source_file(path)?;
    if parsed.language != "java" {
        return Err(anyhow!(
            "analysis.fieldInitializerClosure only supports java files"
        ));
    }

    let all_fields = java_field_facts_for_parsed(&parsed, class_name);
    let static_final_names: BTreeSet<&str> = all_fields
        .iter()
        .filter(|f| f.is_static && f.is_final)
        .map(|f| f.name.as_str())
        .collect();

    // Build initializer refs for ALL static final fields (not just requested
    // ones), so transitive closure can follow dependency chains beyond the
    // originally requested set.
    let mut initializer_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() != "field_declaration" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                stack.push(child);
            }
            continue;
        }
        let owner_class = java_owner_class_name(node, &parsed.source);
        if class_name.is_some_and(|wanted| owner_class.as_deref() != Some(wanted)) {
            continue;
        }
        // Check if static final.
        let (modifiers, _annotations) = java_modifiers_and_annotations(node, &parsed.source);
        let is_static_final =
            modifiers.iter().any(|m| m == "static") && modifiers.iter().any(|m| m == "final");
        if !is_static_final {
            continue;
        }
        // Find the variable_declarator and get name + initializer.
        let mut decl_name: Option<String> = None;
        let mut init_node: Option<tree_sitter::Node<'_>> = None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                decl_name = child
                    .child_by_field_name("name")
                    .map(|n| text_of(&parsed.source, n));
                init_node = child.child_by_field_name("value");
            }
        }
        let Some(field_name) = decl_name else {
            continue;
        };
        let Some(init) = init_node else {
            continue;
        };
        let refs = collect_identifiers_in_subtree(init, &parsed.source);
        let dep_set: BTreeSet<String> = refs
            .into_iter()
            .filter(|r| static_final_names.contains(r.as_str()) && r != &field_name)
            .collect();
        initializer_refs.insert(field_name, dep_set);
    }

    // Compute transitive closure: for each requested field, follow dependency
    // chains until fixed point.
    let requested: BTreeSet<&str> = field_names.iter().map(String::as_str).collect();
    let mut closure: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in &requested {
        let mut deps: BTreeSet<String> = BTreeSet::new();
        let mut frontier: Vec<String> = initializer_refs
            .get(*name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        while let Some(dep) = frontier.pop() {
            if !deps.insert(dep.clone()) {
                continue;
            }
            for transitive in initializer_refs.get(&dep).cloned().unwrap_or_default() {
                if !deps.contains(&transitive) {
                    frontier.push(transitive);
                }
            }
        }
        if !deps.is_empty() {
            let mut sorted: Vec<String> = deps.into_iter().collect();
            sorted.sort();
            closure.insert(name.to_string(), sorted);
        }
    }
    Ok(closure)
}

/// Collect all identifier names in a subtree — used for initializer reference
/// extraction.
fn collect_identifiers_in_subtree(root: tree_sitter::Node<'_>, source: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                ids.insert(text.to_string());
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    ids
}

fn java_field_facts_for_parsed(
    parsed: &super::ParsedSource,
    class_name: Option<&str>,
) -> Vec<JavaFieldFact> {
    let mut fields = Vec::new();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "field_declaration" {
            let owner_class = java_owner_class_name(node, &parsed.source);
            if class_name.is_some_and(|wanted| owner_class.as_deref() != Some(wanted)) {
                continue;
            }
            let type_text = node
                .child_by_field_name("type")
                .map(|child| text_of(&parsed.source, child))
                .unwrap_or_else(|| "?".to_string());
            let (modifiers, annotations) = java_modifiers_and_annotations(node, &parsed.source);
            let visibility = modifiers
                .iter()
                .find(|m| matches!(m.as_str(), "public" | "protected" | "private"))
                .cloned();
            let is_static = modifiers.iter().any(|m| m == "static");
            let is_final = modifiers.iter().any(|m| m == "final");
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() != "variable_declarator" && child.kind() != "variable_declarator_id"
                {
                    continue;
                }
                let Some(name_node) = java_field_name_node(child) else {
                    continue;
                };
                let Some(name) = parsed
                    .source
                    .get(name_node.start_byte()..name_node.end_byte())
                    .map(str::to_string)
                else {
                    continue;
                };
                fields.push(JavaFieldFact {
                    name,
                    type_text: type_text.clone(),
                    owner_class: owner_class.clone(),
                    visibility: visibility.clone(),
                    modifiers: modifiers.clone(),
                    annotations: annotations.clone(),
                    is_static,
                    is_final,
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                    name_byte_start: name_node.start_byte(),
                    name_byte_end: name_node.end_byte(),
                });
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    fields.sort_by(|a, b| {
        a.byte_start
            .cmp(&b.byte_start)
            .then(a.name_byte_start.cmp(&b.name_byte_start))
    });
    fields
}

fn java_constructor_injected_fields(
    parsed: &super::ParsedSource,
    class_name: Option<&str>,
) -> BTreeSet<(Option<String>, String)> {
    let mut injected = BTreeSet::new();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "constructor_declaration" {
            let owner_class = java_owner_class_name(node, &parsed.source);
            if class_name.is_some_and(|wanted| owner_class.as_deref() != Some(wanted)) {
                continue;
            }
            let (_, annotations) = java_modifiers_and_annotations(node, &parsed.source);
            if !annotations
                .iter()
                .any(|annotation| java_annotation_is_inject(annotation))
            {
                continue;
            }
            let params: BTreeSet<String> = node
                .child_by_field_name("parameters")
                .map(|params| {
                    java_params(params, &parsed.source)
                        .into_iter()
                        .filter_map(|param| param.name)
                        .collect()
                })
                .unwrap_or_default();
            if params.is_empty() {
                continue;
            }
            let mut ctor_stack = vec![node];
            while let Some(descendant) = ctor_stack.pop() {
                if descendant.kind() == "assignment_expression"
                    && let Some((field_name, param_name)) =
                        java_this_field_param_assignment(descendant, &parsed.source)
                    && params.contains(&param_name)
                {
                    injected.insert((owner_class.clone(), field_name));
                }
                let mut cursor = descendant.walk();
                for child in descendant.named_children(&mut cursor) {
                    ctor_stack.push(child);
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    injected
}

fn java_this_field_param_assignment(
    assignment: tree_sitter::Node<'_>,
    source: &str,
) -> Option<(String, String)> {
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    let field_name = java_this_field_name(left, source)?;
    let param_name = java_identifier_text(right, source)?;
    Some((field_name, param_name))
}

fn java_this_field_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if node.kind() != "field_access" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    if object.kind() != "this" && object.kind() != "this_expression" {
        return None;
    }
    node.child_by_field_name("field")
        .map(|field| text_of(source, field))
}

fn java_identifier_text(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "identifier" {
        return Some(text_of(source, node));
    }
    None
}

fn java_field_name_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    node.child_by_field_name("name").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
    })
}

fn java_owner_class_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "interface_declaration"
        ) {
            return parent
                .child_by_field_name("name")
                .map(|name| text_of(source, name));
        }
        current = parent.parent();
    }
    None
}

fn java_source_field_access_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let parent = node.parent()?;
    match parent.kind() {
        "variable_declarator"
        | "formal_parameter"
        | "spread_parameter"
        | "catch_formal_parameter"
        | "method_declaration"
        | "constructor_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "enum_declaration"
        | "annotation_type_declaration"
        | "labeled_statement"
        | "type_parameter"
        | "marker_annotation"
        | "annotation"
        | "enum_constant"
        | "method_invocation" => {
            if parent.child_by_field_name("name").map(|child| child.id()) == Some(node.id()) {
                return None;
            }
        }
        "field_access" => {
            if parent.child_by_field_name("field").map(|child| child.id()) == Some(node.id()) {
                let object = parent.child_by_field_name("object")?;
                if object.kind() == "this" || object.kind() == "this_expression" {
                    return Some(parent);
                }
                return None;
            }
        }
        "scoped_identifier" | "scoped_type_identifier" | "type_identifier" | "generic_type" => {
            return None;
        }
        _ => {}
    }
    Some(node)
}

fn java_identifier_shadowed(node: tree_sitter::Node<'_>, name: &str, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(scope) = current {
        if scope.kind() == "class_body" {
            return false;
        }
        if matches!(
            scope.kind(),
            "block" | "method_declaration" | "constructor_declaration" | "lambda_expression"
        ) && java_scope_declares_before(scope, name, node.start_byte(), source)
        {
            return true;
        }
        current = scope.parent();
    }
    false
}

fn java_scope_declares_before(
    scope: tree_sitter::Node<'_>,
    name: &str,
    before: usize,
    source: &str,
) -> bool {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.start_byte() >= before {
            continue;
        }
        if matches!(
            node.kind(),
            "formal_parameter"
                | "spread_parameter"
                | "catch_formal_parameter"
                | "local_variable_declaration"
                | "resource"
        ) && java_declares_name(node, name, source)
        {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn java_declares_name(node: tree_sitter::Node<'_>, name: &str, source: &str) -> bool {
    if let Some(name_node) = node.child_by_field_name("name") {
        return text_of(source, name_node) == name;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        if child.kind() == "variable_declarator"
            && let Some(name_node) = java_field_name_node(child)
        {
            return text_of(source, name_node) == name;
        }
        false
    })
}

fn java_access_kind(mut access_node: tree_sitter::Node<'_>) -> &'static str {
    while let Some(parent) = access_node.parent() {
        match parent.kind() {
            "assignment_expression" => {
                if parent.child_by_field_name("left").map(|child| child.id())
                    == Some(access_node.id())
                {
                    return "write";
                }
                return "read";
            }
            "update_expression" => return "write",
            "parenthesized_expression" | "cast_expression" => {
                access_node = parent;
            }
            _ => return "read",
        }
    }
    "read"
}

fn java_enclosing_callable_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "method_declaration" || n.kind() == "constructor_declaration" {
            return n
                .child_by_field_name("name")
                .map(|name| text_of(source, name));
        }
        current = n.parent();
    }
    None
}

fn source_line_col(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte.min(source.len())];
    let line = prefix.as_bytes().iter().filter(|b| **b == b'\n').count() + 1;
    let col = prefix
        .rsplit_once('\n')
        .map(|(_, tail)| tail.chars().count() + 1)
        .unwrap_or_else(|| prefix.chars().count() + 1);
    (line, col)
}

fn source_line_context(source: &str, byte: usize) -> String {
    let start = source[..byte.min(source.len())]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let end = source[byte.min(source.len())..]
        .find('\n')
        .map(|idx| byte.min(source.len()) + idx)
        .unwrap_or(source.len());
    source[start..end].trim().to_string()
}

/// Byte range of the `name` identifier of the item at (or enclosing) the
/// given byte range — e.g. the `spawn_worker` of `pub fn spawn_worker(...)`.
/// Lets position-sensitive consumers (LSP rename) accept whole-item spans:
/// aiming at an item's `byte_start` hits the `pub` keyword, which
/// rust-analyzer refuses with "No references found at position".
pub fn name_span(
    path: &Path,
    byte_start: usize,
    byte_end: usize,
) -> Result<Option<(usize, usize)>> {
    let parsed = super::parse_source_file(path)?;
    let len = parsed.source.len();
    let (start, end) = (
        byte_start.min(len),
        byte_end.min(len).max(byte_start.min(len)),
    );
    let mut node = match parsed
        .tree
        .root_node()
        .named_descendant_for_byte_range(start, end)
    {
        Some(node) => node,
        None => return Ok(None),
    };
    loop {
        if node.kind() == "identifier" {
            return Ok(Some((node.start_byte(), node.end_byte())));
        }
        if let Some(name) = node.child_by_field_name("name") {
            return Ok(Some((name.start_byte(), name.end_byte())));
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return Ok(None),
        }
    }
}

/// One enumerated source file.
#[derive(Debug, Clone)]
pub struct SourceFileFact {
    /// Path relative to the enumeration root.
    pub file: String,
    pub language: &'static str,
}

/// Cap on enumerated files, mirroring [`MAX_QUERY_CAPTURES`]'s shape: callers
/// surface a `truncated` flag instead of unbounded payloads.
pub const MAX_SOURCE_FILES: usize = 5_000;

/// Enumerate the tree-sitter-supported source files under `root` (optionally
/// narrowed to `dir`, a root-relative subdirectory, and/or one `language`
/// name as returned by the other facts). Pure walk of the working set:
/// skips dot-directories and the conventional build/vendor dirs (`target`,
/// `node_modules`, `build`, `dist`, `vendor`). Results are sorted, capped at
/// [`MAX_SOURCE_FILES`] (the bool is `truncated`).
// Blocking walk by design, like every facts function: callers (the code.files
// binding) run it on the blocking pool via call_blocking.
#[allow(clippy::disallowed_methods)]
pub fn source_files(
    root: &Path,
    dir: Option<&str>,
    language: Option<&str>,
) -> Result<(Vec<SourceFileFact>, bool)> {
    const SKIP_DIRS: &[&str] = &["target", "node_modules", "build", "dist", "vendor"];
    let base = match dir {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    };
    if !base.is_dir() {
        return Err(anyhow!(
            "`{}` is not a directory under the workspace root",
            dir.unwrap_or(".")
        ));
    }
    let mut found = Vec::new();
    let mut stack = vec![base];
    let mut truncated = false;
    'walk: while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| anyhow!("reading {}: {e}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| anyhow!("reading {}: {e}", current.display()))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let Some(lang) = chunker::code::language_for_path(&path) else {
                continue;
            };
            if let Some(filter) = language
                && lang != filter
            {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            found.push(SourceFileFact {
                file: rel,
                language: lang,
            });
            if found.len() >= MAX_SOURCE_FILES {
                truncated = true;
                break 'walk;
            }
        }
    }
    found.sort_by(|a, b| a.file.cmp(&b.file));
    Ok((found, truncated))
}

/// Parse health of one source file.
#[derive(Debug, Clone)]
pub struct ParseCheckFacts {
    pub language: &'static str,
    pub error_nodes: usize,
    pub missing_nodes: usize,
}

/// Tree-sitter parse check for `path` — the post-apply validation primitive.
/// Errors for unsupported extensions; callers skip validation for those.
pub fn parse_check(path: &Path) -> Result<ParseCheckFacts> {
    let parsed = super::parse_source_file(path)?;
    let report = super::parse_report(parsed.tree.root_node());
    Ok(ParseCheckFacts {
        language: parsed.language,
        error_nodes: report.error_nodes,
        missing_nodes: report.missing_nodes,
    })
}

/// A byte range extracted from a source signature fact.
#[derive(Debug, Clone)]
pub struct SignatureRangeFact {
    pub byte_start: usize,
    pub byte_end: usize,
    pub content_sha256: String,
}

/// One parameter of a Rust function signature.
#[derive(Debug, Clone)]
pub struct FnParamFact {
    /// Binding pattern text (`raw`, `mut workers`, `&self`, …).
    pub pattern: String,
    /// Declared type text; `None` for `self` parameters.
    pub type_text: Option<String>,
}

/// Signature facts for one function item, extracted from the AST.
#[derive(Debug, Clone)]
pub struct FnSignatureFacts {
    pub language: &'static str,
    pub kind: String,
    pub name: Option<String>,
    /// Visibility modifier text (`pub`, `pub(crate)`, …); `None` = private.
    pub visibility: Option<String>,
    pub is_async: bool,
    pub params: Vec<FnParamFact>,
    /// Return type text without the `->`; `None` = unit.
    pub return_type: Option<String>,
    /// Generic parameter list text (`<T: Clone>`); `None` when absent.
    pub generics: Option<String>,
    /// Byte range of the resolved function item (may widen a narrower input
    /// span, e.g. a name identifier, to the whole item).
    pub byte_start: usize,
    pub byte_end: usize,
    pub signature_span: SignatureRangeFact,
    pub params_span: Option<SignatureRangeFact>,
    pub content_sha256: String,
}

/// One Java formal parameter in a method/constructor signature.
#[derive(Debug, Clone)]
pub struct JavaParamFact {
    pub name: Option<String>,
    pub type_text: Option<String>,
    pub modifiers: Vec<String>,
    pub annotations: Vec<String>,
    pub varargs: bool,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Java method/constructor signature facts.
#[derive(Debug, Clone)]
pub struct JavaSignatureFacts {
    pub language: &'static str,
    pub kind: String,
    pub name: Option<String>,
    pub visibility: Option<String>,
    pub modifiers: Vec<String>,
    pub annotations: Vec<String>,
    pub params: Vec<JavaParamFact>,
    pub return_type: Option<String>,
    pub type_parameters: Option<String>,
    pub throws: Vec<String>,
    pub throws_text: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub signature_span: SignatureRangeFact,
    pub params_span: Option<SignatureRangeFact>,
    pub content_sha256: String,
}

/// Language-dispatched callable declaration signature.
#[derive(Debug, Clone)]
pub enum SignatureFacts {
    Rust(FnSignatureFacts),
    Java(JavaSignatureFacts),
}

trait SignatureExtractor {
    fn extract(parsed: &super::ParsedSource, start: usize, end: usize) -> Result<SignatureFacts>;
}

struct RustSignatureExtractor;
struct JavaSignatureExtractor;

/// Extract the signature of the callable item at (or enclosing) the given
/// byte range. Rust returns Rust-shaped function facts; Java returns
/// method/constructor-shaped facts. Unsupported languages fail closed with a
/// clear error rather than guessing at grammar shapes.
///
/// When `expected_content_sha256` is set, the file hash is verified BEFORE
/// the byte range is interpreted — a drifted file must fail as `stale_span`,
/// never as a confusing "no function_item at range" against the new tree.
pub fn callable_signature(
    path: &Path,
    byte_start: usize,
    byte_end: usize,
    expected_content_sha256: Option<&str>,
) -> Result<SignatureFacts> {
    let parsed = super::parse_source_file(path)?;
    if let Some(expected) = expected_content_sha256 {
        let current = sha256_hex(parsed.source.as_bytes());
        if current != expected {
            return Err(anyhow!(
                "stale_span: {} changed since the span was minted (span hash {expected}, current {current}); re-derive the span from fresh facts",
                path.display()
            ));
        }
    }
    let len = parsed.source.len();
    let (start, end) = (
        byte_start.min(len),
        byte_end.min(len).max(byte_start.min(len)),
    );
    match parsed.language {
        "rust" => RustSignatureExtractor::extract(&parsed, start, end),
        "java" => JavaSignatureExtractor::extract(&parsed, start, end),
        other => Err(anyhow!("code.signature does not support {other}")),
    }
}

fn range_fact(byte_start: usize, byte_end: usize, content_sha256: &str) -> SignatureRangeFact {
    SignatureRangeFact {
        byte_start,
        byte_end,
        content_sha256: content_sha256.to_string(),
    }
}

fn text_of(source: &str, node: tree_sitter::Node<'_>) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .to_string()
}

fn trim_signature_end(source: &str, mut end: usize, floor: usize) -> usize {
    while end > floor
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    end
}

fn node_at_or_enclosing<'tree>(
    parsed: &'tree super::ParsedSource,
    start: usize,
    end: usize,
    kinds: &[&str],
) -> Result<tree_sitter::Node<'tree>> {
    let root = parsed.tree.root_node();
    let mut node = root
        .named_descendant_for_byte_range(start, end)
        .ok_or_else(|| anyhow!("no syntax node at byte range {start}..{end}"))?;
    while !kinds.contains(&node.kind()) {
        let Some(parent) = node.parent() else {
            return Err(anyhow!(
                "no {} at or enclosing byte range {start}..{end} (innermost node kind: {})",
                kinds.join("/"),
                parsed
                    .tree
                    .root_node()
                    .named_descendant_for_byte_range(start, end)
                    .map(|n| n.kind())
                    .unwrap_or("?")
            ));
        };
        node = parent;
    }
    Ok(node)
}

impl SignatureExtractor for RustSignatureExtractor {
    fn extract(parsed: &super::ParsedSource, start: usize, end: usize) -> Result<SignatureFacts> {
        let node = node_at_or_enclosing(parsed, start, end, &["function_item"])?;
        let content_sha256 = sha256_hex(parsed.source.as_bytes());

        let mut visibility = None;
        let mut is_async = false;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "visibility_modifier" => visibility = Some(text_of(&parsed.source, child)),
                "function_modifiers" => is_async = text_of(&parsed.source, child).contains("async"),
                _ => {}
            }
        }

        let mut params = Vec::new();
        let params_node = node.child_by_field_name("parameters");
        if let Some(parameters) = params_node {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                match parameter.kind() {
                    "parameter" => params.push(FnParamFact {
                        pattern: parameter
                            .child_by_field_name("pattern")
                            .map(|node| text_of(&parsed.source, node))
                            .unwrap_or_else(|| text_of(&parsed.source, parameter)),
                        type_text: parameter
                            .child_by_field_name("type")
                            .map(|node| text_of(&parsed.source, node)),
                    }),
                    "self_parameter" => params.push(FnParamFact {
                        pattern: text_of(&parsed.source, parameter),
                        type_text: None,
                    }),
                    "attribute_item" | "line_comment" | "block_comment" => {}
                    _ => params.push(FnParamFact {
                        pattern: text_of(&parsed.source, parameter),
                        type_text: None,
                    }),
                }
            }
        }

        let signature_end = node
            .child_by_field_name("body")
            .map(|body| trim_signature_end(&parsed.source, body.start_byte(), node.start_byte()))
            .unwrap_or(node.end_byte());
        Ok(SignatureFacts::Rust(FnSignatureFacts {
            language: "rust",
            kind: node.kind().to_string(),
            name: node
                .child_by_field_name("name")
                .map(|node| text_of(&parsed.source, node)),
            visibility,
            is_async,
            params,
            return_type: node
                .child_by_field_name("return_type")
                .map(|node| text_of(&parsed.source, node)),
            generics: node
                .child_by_field_name("type_parameters")
                .map(|node| text_of(&parsed.source, node)),
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            signature_span: range_fact(node.start_byte(), signature_end, &content_sha256),
            params_span: params_node
                .map(|node| range_fact(node.start_byte(), node.end_byte(), &content_sha256)),
            content_sha256,
        }))
    }
}

impl SignatureExtractor for JavaSignatureExtractor {
    fn extract(parsed: &super::ParsedSource, start: usize, end: usize) -> Result<SignatureFacts> {
        let node = node_at_or_enclosing(
            parsed,
            start,
            end,
            &["method_declaration", "constructor_declaration"],
        )?;
        let content_sha256 = sha256_hex(parsed.source.as_bytes());
        let params_node = node.child_by_field_name("parameters");
        let (modifiers, annotations) = java_modifiers_and_annotations(node, &parsed.source);
        let visibility = modifiers
            .iter()
            .find(|m| matches!(m.as_str(), "public" | "protected" | "private"))
            .cloned();
        let signature_end = node
            .child_by_field_name("body")
            .map(|body| trim_signature_end(&parsed.source, body.start_byte(), node.start_byte()))
            .unwrap_or(node.end_byte());
        let throws_text = child_text_by_kind(node, "throws", &parsed.source);
        let throws = throws_text
            .as_deref()
            .map(parse_java_throws)
            .unwrap_or_default();
        let params = params_node
            .map(|params| java_params(params, &parsed.source))
            .unwrap_or_default();

        Ok(SignatureFacts::Java(JavaSignatureFacts {
            language: "java",
            kind: node.kind().to_string(),
            name: node
                .child_by_field_name("name")
                .map(|node| text_of(&parsed.source, node)),
            visibility,
            modifiers,
            annotations,
            params,
            return_type: if node.kind() == "method_declaration" {
                node.child_by_field_name("type")
                    .map(|node| text_of(&parsed.source, node))
            } else {
                None
            },
            type_parameters: child_text_by_kind(node, "type_parameters", &parsed.source),
            throws,
            throws_text,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            signature_span: range_fact(node.start_byte(), signature_end, &content_sha256),
            params_span: params_node
                .map(|node| range_fact(node.start_byte(), node.end_byte(), &content_sha256)),
            content_sha256,
        }))
    }
}

fn child_text_by_kind(node: tree_sitter::Node<'_>, kind: &str, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
        .map(|child| text_of(source, child))
}

fn java_modifiers_and_annotations(
    node: tree_sitter::Node<'_>,
    source: &str,
) -> (Vec<String>, Vec<String>) {
    let mut modifiers = Vec::new();
    let mut annotations = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            if !matches!(
                child.kind(),
                "marker_annotation" | "annotation" | "public" | "protected" | "private"
            ) {
                break;
            }
            collect_java_modifier_child(child, source, &mut modifiers, &mut annotations);
            continue;
        }
        let mut modifier_cursor = child.walk();
        for modifier_child in child.children(&mut modifier_cursor) {
            collect_java_modifier_child(modifier_child, source, &mut modifiers, &mut annotations);
        }
        break;
    }
    (modifiers, annotations)
}

fn collect_java_modifier_child(
    node: tree_sitter::Node<'_>,
    source: &str,
    modifiers: &mut Vec<String>,
    annotations: &mut Vec<String>,
) {
    match node.kind() {
        "public" | "protected" | "private" | "static" | "final" | "abstract" | "synchronized"
        | "native" | "strictfp" | "default" | "transient" | "volatile" => {
            modifiers.push(node.kind().to_string());
        }
        "marker_annotation" | "annotation" => annotations.push(text_of(source, node)),
        _ => {}
    }
}

fn java_annotation_is_inject(annotation: &str) -> bool {
    let annotation = annotation.trim().trim_start_matches('@');
    annotation == "Inject"
        || annotation.starts_with("Inject(")
        || annotation.ends_with(".Inject")
        || annotation.contains(".Inject(")
}

fn java_params(params: tree_sitter::Node<'_>, source: &str) -> Vec<JavaParamFact> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "formal_parameter" | "spread_parameter" | "receiver_parameter"
        ) {
            continue;
        }
        let (modifiers, annotations) = java_modifiers_and_annotations(child, source);
        out.push(JavaParamFact {
            name: child
                .child_by_field_name("name")
                .map(|node| text_of(source, node)),
            type_text: child
                .child_by_field_name("type")
                .map(|node| text_of(source, node)),
            modifiers,
            annotations,
            varargs: child.kind() == "spread_parameter",
            byte_start: child.start_byte(),
            byte_end: child.end_byte(),
        });
    }
    out
}

fn parse_java_throws(throws_text: &str) -> Vec<String> {
    throws_text
        .trim()
        .strip_prefix("throws")
        .unwrap_or(throws_text)
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod facts_tests {
    use super::*;
    use std::fs;

    fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("probe.rs");
        fs::write(
            &path,
            "pub struct Alpha;\n\npub fn beta() -> u8 {\n    7\n}\n\nfn gamma() {}\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn file_items_inventories_rust_top_level_items() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let facts = file_items(&path).unwrap();
        assert_eq!(facts.language, "rust");
        assert_eq!(facts.content_sha256.len(), 64);
        assert_eq!(facts.source_len, fs::read(&path).unwrap().len());
        let names: Vec<_> = facts
            .items
            .iter()
            .filter_map(|i| i.item.name.as_deref())
            .collect();
        assert!(names.contains(&"Alpha"), "items: {names:?}");
        assert!(names.contains(&"beta"), "items: {names:?}");
        let beta = facts
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("beta"))
            .unwrap();
        assert_eq!(beta.visibility.as_deref(), Some("pub"));
        let gamma = facts
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("gamma"))
            .unwrap();
        assert_eq!(gamma.visibility, None);
    }

    #[test]
    fn file_items_marks_java_declaring_type_and_nested_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("OrderView.java");
        fs::write(
            &path,
            r#"package com.acme;

class OrderView {
    void refresh() {}

    record Row(String id) {
        String label() {
            return id;
        }
    }
}
"#,
        )
        .unwrap();

        let facts = file_items(&path).unwrap();
        let refresh = facts
            .items
            .iter()
            .find(|item| item.item.name.as_deref() == Some("refresh"))
            .expect("refresh item");
        assert_eq!(refresh.declaring_type.as_deref(), Some("OrderView"));
        assert!(!refresh.nested);

        let label = facts
            .items
            .iter()
            .find(|item| item.item.name.as_deref() == Some("label"))
            .expect("label item");
        assert_eq!(label.declaring_type.as_deref(), Some("Row"));
        assert!(label.nested);
    }

    #[test]
    fn file_query_returns_named_captures_with_spans() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let facts = file_query(&path, "(function_item name: (identifier) @fn_name)", None).unwrap();
        let names: Vec<_> = facts.captures.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["beta", "gamma"]);
        let beta = &facts.captures[0];
        assert_eq!(beta.capture, "fn_name");
        assert_eq!(beta.kind, "identifier");
        let source = fs::read_to_string(&path).unwrap();
        assert_eq!(&source[beta.byte_start..beta.byte_end], "beta");
    }

    #[test]
    fn file_query_within_restricts_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let source = fs::read_to_string(&path).unwrap();
        let gamma_at = source.find("fn gamma").unwrap();
        let facts = file_query(
            &path,
            "(function_item name: (identifier) @fn_name)",
            Some((gamma_at, source.len())),
        )
        .unwrap();
        let names: Vec<_> = facts.captures.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["gamma"]);
    }

    #[test]
    fn fn_signature_extracts_pub_fn_with_result_return() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("sig.rs");
        fs::write(
            &path,
            "pub async fn fetch<T: Clone>(id: u32, name: &str) -> Result<T, String> {\n    todo!()\n}\n\nfn private_unit(x: u8) {}\n\npub struct S;\nimpl S {\n    pub fn method(&self, n: usize) -> usize { n }\n}\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();

        let at = source.find("fetch").unwrap();
        let SignatureFacts::Rust(sig) = callable_signature(&path, at, at + 5, None).unwrap() else {
            panic!("expected rust signature");
        };
        assert_eq!(sig.name.as_deref(), Some("fetch"));
        assert_eq!(sig.visibility.as_deref(), Some("pub"));
        assert!(sig.is_async);
        assert_eq!(sig.generics.as_deref(), Some("<T: Clone>"));
        assert_eq!(sig.return_type.as_deref(), Some("Result<T, String>"));
        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.params[0].pattern, "id");
        assert_eq!(sig.params[0].type_text.as_deref(), Some("u32"));
        assert_eq!(sig.params[1].type_text.as_deref(), Some("&str"));
        assert_eq!(
            &source[sig.byte_start..sig.byte_end]
                .split('(')
                .next()
                .unwrap(),
            &"pub async fn fetch<T: Clone>"
        );

        let at = source.find("private_unit").unwrap();
        let SignatureFacts::Rust(sig) = callable_signature(&path, at, at, None).unwrap() else {
            panic!("expected rust signature");
        };
        assert_eq!(sig.visibility, None);
        assert_eq!(sig.return_type, None);
        assert!(!sig.is_async);

        let at = source.find("method").unwrap();
        let SignatureFacts::Rust(sig) = callable_signature(&path, at, at + 6, None).unwrap() else {
            panic!("expected rust signature");
        };
        assert_eq!(sig.name.as_deref(), Some("method"));
        assert_eq!(sig.params[0].pattern, "&self");
        assert_eq!(sig.params[0].type_text, None);
        assert_eq!(sig.return_type.as_deref(), Some("usize"));
    }

    #[test]
    fn fn_signature_rejects_non_function_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let source = fs::read_to_string(&path).unwrap();
        let at = source.find("struct Alpha").unwrap();
        let err = callable_signature(&path, at, at + 5, None).unwrap_err();
        assert!(err.to_string().contains("no function_item"), "got: {err}");
    }

    #[test]
    fn file_query_rejects_invalid_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let err = file_query(&path, "(nonsense_node_kind) @x", None).unwrap_err();
        assert!(err.to_string().contains("query"), "got: {err}");
    }
}
