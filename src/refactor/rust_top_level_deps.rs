//! `rust_top_level_dependency_analysis` plan kind.
//!
//! Analysis-only support for choosing cohesive top-level Rust item clusters
//! before running `extract_rust_items_to_submodule`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tree_sitter::Node;
use walkdir::WalkDir;

use super::{
    SemanticStatus, SyntaxItem, parse_rust_file, path_string, resolve_project_dir, rust_items,
};

const MAX_EXTERNAL_REFS_PER_ITEM: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopLevelDependencyGraph {
    pub items: Vec<TopLevelItemNode>,
    pub edges: Vec<TopLevelDependencyEdge>,
    pub external_references: Vec<TopLevelExternalReference>,
    pub suggested_clusters: Vec<TopLevelSuggestedCluster>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopLevelItemNode {
    pub name: String,
    pub kind: String,
    pub line_start: usize,
    pub line_end: usize,
    pub calls: Vec<String>,
    pub type_refs: Vec<String>,
    pub module_refs: Vec<String>,
    pub global_refs: Vec<String>,
    pub attrs: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopLevelDependencyEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopLevelExternalReference {
    pub item: String,
    pub path: String,
    pub line: usize,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopLevelSuggestedCluster {
    pub id: String,
    pub items: Vec<String>,
    pub reason: String,
    pub internal_edges: usize,
    pub external_references: usize,
}

pub fn analyze_top_level(
    source_path: &Path,
    project_dir: Option<&str>,
    item_names: Option<&[String]>,
    item_kinds: Option<&[String]>,
) -> Result<TopLevelDependencyGraph> {
    let parsed = parse_rust_file(source_path)?;
    let all_items = rust_items(&parsed);
    let selected = select_named_top_level_items(&all_items, item_names, item_kinds)?;
    let selected_names = selected
        .iter()
        .filter_map(|item| item.name.clone())
        .collect::<BTreeSet<_>>();
    let item_kinds_by_name = all_items
        .iter()
        .filter_map(|item| {
            item.name
                .as_ref()
                .map(|name| (name.clone(), item.kind.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut nodes = Vec::new();
    let mut edge_set = BTreeSet::new();
    let root = parsed.tree.root_node();
    for item in selected {
        let Some(name) = item.name.as_deref() else {
            continue;
        };
        let Some(node) = root.named_descendant_for_byte_range(item.byte_start, item.byte_end)
        else {
            continue;
        };
        let refs = collect_item_refs(
            node,
            &parsed.source,
            name,
            &selected_names,
            &item_kinds_by_name,
        );
        for target in &refs.calls {
            edge_set.insert((name.to_string(), target.clone(), "calls".to_string()));
        }
        for target in &refs.type_refs {
            edge_set.insert((name.to_string(), target.clone(), "type_ref".to_string()));
        }
        for target in &refs.module_refs {
            edge_set.insert((name.to_string(), target.clone(), "module_ref".to_string()));
        }
        for target in &refs.global_refs {
            edge_set.insert((name.to_string(), target.clone(), "global_ref".to_string()));
        }

        let mut warnings = Vec::new();
        if refs.macro_invocations > 0 {
            warnings.push(format!(
                "macro-heavy: {} macro invocation(s) may hide dependency edges",
                refs.macro_invocations
            ));
        }
        nodes.push(TopLevelItemNode {
            name: name.to_string(),
            kind: item.kind.clone(),
            line_start: item.line_start,
            line_end: item.line_end,
            calls: refs.calls.into_iter().collect(),
            type_refs: refs.type_refs.into_iter().collect(),
            module_refs: refs.module_refs.into_iter().collect(),
            global_refs: refs.global_refs.into_iter().collect(),
            attrs: item.attributes.clone(),
            warnings,
        });
    }

    let edges = edge_set
        .into_iter()
        .map(|(from, to, kind)| TopLevelDependencyEdge { from, to, kind })
        .collect::<Vec<_>>();
    let external_references =
        collect_external_references(source_path, project_dir, &selected_names)?;
    let suggested_clusters = suggest_clusters(&nodes, &edges, &external_references);
    let warnings = graph_warnings(&nodes);

    Ok(TopLevelDependencyGraph {
        items: nodes,
        edges,
        external_references,
        suggested_clusters,
        warnings,
    })
}

pub fn semantic_status() -> SemanticStatus {
    SemanticStatus::IndexedHints
}

fn select_named_top_level_items<'a>(
    items: &'a [SyntaxItem],
    names: Option<&[String]>,
    kinds: Option<&[String]>,
) -> Result<Vec<&'a SyntaxItem>> {
    let kind_set = kinds.map(|xs| xs.iter().map(String::as_str).collect::<BTreeSet<_>>());
    let mut candidates = items
        .iter()
        .filter(|item| item.name.is_some())
        .filter(|item| match &kind_set {
            Some(set) => set.contains(item.kind.as_str()),
            None => true,
        })
        .collect::<Vec<_>>();

    if let Some(names) = names.filter(|xs| !xs.is_empty()) {
        let mut selected = Vec::new();
        for expected in names {
            let matches = candidates
                .iter()
                .copied()
                .filter(|item| item.name.as_deref() == Some(expected.as_str()))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => anyhow::bail!("requested Rust item `{expected}` was not found"),
                [item] => selected.push(*item),
                _ => anyhow::bail!(
                    "requested Rust item `{expected}` matched multiple declarations; narrow with item_kinds"
                ),
            }
        }
        candidates = selected;
    }

    candidates.sort_by_key(|item| item.byte_start);
    Ok(candidates)
}

#[derive(Default)]
struct ItemRefs {
    calls: BTreeSet<String>,
    type_refs: BTreeSet<String>,
    module_refs: BTreeSet<String>,
    global_refs: BTreeSet<String>,
    macro_invocations: usize,
}

fn collect_item_refs(
    node: Node,
    source: &str,
    self_name: &str,
    selected_names: &BTreeSet<String>,
    item_kinds_by_name: &BTreeMap<String, String>,
) -> ItemRefs {
    let mut refs = ItemRefs::default();
    collect_item_refs_inner(
        node,
        source,
        self_name,
        selected_names,
        item_kinds_by_name,
        &mut refs,
    );
    refs
}

fn collect_item_refs_inner(
    node: Node,
    source: &str,
    self_name: &str,
    selected_names: &BTreeSet<String>,
    item_kinds_by_name: &BTreeMap<String, String>,
    refs: &mut ItemRefs,
) {
    match node.kind() {
        "macro_invocation" => {
            refs.macro_invocations += 1;
            return;
        }
        "call_expression" => {
            if let Some(function) = node.child_by_field_name("function") {
                collect_call_ref(function, source, self_name, selected_names, refs);
            }
        }
        "identifier" | "type_identifier" | "scoped_identifier" | "generic_type" => {
            for ident in identifiers_in_node(node, source) {
                classify_named_ref(&ident, self_name, selected_names, item_kinds_by_name, refs);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_item_refs_inner(
            child,
            source,
            self_name,
            selected_names,
            item_kinds_by_name,
            refs,
        );
    }
}

fn collect_call_ref(
    function: Node,
    source: &str,
    self_name: &str,
    selected_names: &BTreeSet<String>,
    refs: &mut ItemRefs,
) {
    let names = identifiers_in_node(function, source);
    for name in names {
        if name != self_name && selected_names.contains(&name) {
            refs.calls.insert(name);
        }
    }
}

fn classify_named_ref(
    ident: &str,
    self_name: &str,
    selected_names: &BTreeSet<String>,
    item_kinds_by_name: &BTreeMap<String, String>,
    refs: &mut ItemRefs,
) {
    if ident == self_name || !selected_names.contains(ident) {
        return;
    }
    match item_kinds_by_name.get(ident).map(String::as_str) {
        Some("mod_item") => {
            refs.module_refs.insert(ident.to_string());
        }
        Some("const_item" | "static_item") => {
            refs.global_refs.insert(ident.to_string());
        }
        Some("function_item") => {
            refs.calls.insert(ident.to_string());
        }
        Some(_) => {
            refs.type_refs.insert(ident.to_string());
        }
        None => {}
    }
}

fn identifiers_in_node(node: Node, source: &str) -> Vec<String> {
    if node.kind() == "identifier" || node.kind() == "type_identifier" {
        return node
            .utf8_text(source.as_bytes())
            .ok()
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        out.extend(identifiers_in_node(child, source));
    }
    out
}

fn collect_external_references(
    source_path: &Path,
    project_dir: Option<&str>,
    item_names: &BTreeSet<String>,
) -> Result<Vec<TopLevelExternalReference>> {
    if item_names.is_empty() {
        return Ok(Vec::new());
    }
    let project_root = match project_dir {
        Some(dir) => resolve_project_dir(Some(dir))?,
        None => source_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let source_path = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let mut counts = BTreeMap::<String, usize>::new();
    let mut refs = Vec::new();

    for entry in WalkDir::new(project_root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "target" | "node_modules" | ".claude" | ".bro"
            )
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|e| e.to_str()) != Some("rs")
        {
            continue;
        }
        let path = entry.path();
        if path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) == source_path {
            continue;
        }
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for (idx, line) in content.lines().enumerate() {
            for item in item_names {
                if !line_contains_word(line, item) {
                    continue;
                }
                let count = counts.entry(item.clone()).or_default();
                if *count >= MAX_EXTERNAL_REFS_PER_ITEM {
                    continue;
                }
                *count += 1;
                refs.push(TopLevelExternalReference {
                    item: item.clone(),
                    path: path_string(path),
                    line: idx + 1,
                    context: line.trim().chars().take(240).collect(),
                });
            }
        }
    }

    refs.sort_by(|a, b| {
        a.item
            .cmp(&b.item)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    Ok(refs)
}

fn line_contains_word(line: &str, needle: &str) -> bool {
    let bytes = line.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || needle_bytes.len() > bytes.len() {
        return false;
    }
    bytes
        .windows(needle_bytes.len())
        .enumerate()
        .any(|(idx, window)| {
            window == needle_bytes
                && is_word_boundary(bytes.get(idx.wrapping_sub(1)).copied())
                && is_word_boundary(bytes.get(idx + needle_bytes.len()).copied())
        })
}

fn is_word_boundary(ch: Option<u8>) -> bool {
    !matches!(ch, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn suggest_clusters(
    nodes: &[TopLevelItemNode],
    edges: &[TopLevelDependencyEdge],
    external_refs: &[TopLevelExternalReference],
) -> Vec<TopLevelSuggestedCluster> {
    let mut parent = nodes
        .iter()
        .map(|node| (node.name.clone(), node.name.clone()))
        .collect::<BTreeMap<_, _>>();
    for edge in edges {
        union(&mut parent, &edge.from, &edge.to);
    }
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in nodes {
        let root = find(&mut parent, &node.name);
        groups.entry(root).or_default().push(node.name.clone());
    }

    groups
        .into_values()
        .enumerate()
        .map(|(idx, mut items)| {
            items.sort();
            let item_set = items.iter().cloned().collect::<BTreeSet<_>>();
            let internal_edges = edges
                .iter()
                .filter(|edge| item_set.contains(&edge.from) && item_set.contains(&edge.to))
                .count();
            let external_references = external_refs
                .iter()
                .filter(|reference| item_set.contains(&reference.item))
                .count();
            let reason = if internal_edges > 0 {
                format!("{internal_edges} internal dependency edge(s)")
            } else {
                "isolated item; no internal dependency edges found".to_string()
            };
            TopLevelSuggestedCluster {
                id: format!("cluster-{}", idx + 1),
                items,
                reason,
                internal_edges,
                external_references,
            }
        })
        .collect()
}

fn find(parent: &mut BTreeMap<String, String>, item: &str) -> String {
    let current = parent
        .get(item)
        .cloned()
        .unwrap_or_else(|| item.to_string());
    if current == item {
        return current;
    }
    let root = find(parent, &current);
    parent.insert(item.to_string(), root.clone());
    root
}

fn union(parent: &mut BTreeMap<String, String>, a: &str, b: &str) {
    let root_a = find(parent, a);
    let root_b = find(parent, b);
    if root_a != root_b {
        parent.insert(root_b, root_a);
    }
}

fn graph_warnings(nodes: &[TopLevelItemNode]) -> Vec<String> {
    let macro_items = nodes
        .iter()
        .filter(|node| !node.warnings.is_empty())
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();
    if macro_items.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "macro-heavy items may have unresolved hidden edges: {}",
            macro_items.join(", ")
        )]
    }
}
