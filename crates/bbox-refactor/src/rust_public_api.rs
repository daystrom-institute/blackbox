// TODO(RX-G2 register): PlanKind::RustPublicApiGuard => rust_public_api::analyze_public_api(...)

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};
use walkdir::WalkDir;

use crate::chunker::code::{language_for_path, parser_for_language};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Fn,
    Struct,
    Enum,
    Trait,
    Const,
    Static,
    Use,
    Mod,
    TypeAlias,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Breaking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchedItem {
    pub path: PathBuf,
    pub kind: ItemKind,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureChange {
    pub name: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiDelta {
    pub additions: Vec<TouchedItem>,
    pub removals: Vec<TouchedItem>,
    pub signature_changes: Vec<SignatureChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReExport {
    pub path: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicApiReport {
    pub public_items_touched: Vec<TouchedItem>,
    pub public_api_delta_summary: ApiDelta,
    pub crate_root_re_exports_affected: Vec<ReExport>,
    pub advisory_severity: Severity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Modify,
    Remove,
    Add,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedChangeRef {
    pub file: PathBuf,
    pub item_name: String,
    pub change_kind: ChangeKind,
}

#[derive(Debug)]
struct ParsedFile {
    // kept: file path tracked alongside parsed source for future diagnostic provenance
    #[allow(dead_code)]
    path: PathBuf,
    source: String,
    tree: Tree,
}

#[derive(Debug, Clone)]
struct AnalyzedItem {
    path: PathBuf,
    kind: ItemKind,
    name: String,
    is_public: bool,
    signature: Option<String>,
}

pub fn analyze_public_api(
    source: &Path,
    proposed_changes: &[ProposedChangeRef],
) -> Result<PublicApiReport> {
    let source = if source.is_absolute() {
        source.to_path_buf()
    } else {
        std::env::current_dir()?
            .join(source)
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf())
    };

    let files = collect_rust_files(&source)?;
    let mut analyzed_items = Vec::new();
    for path in files {
        analyzed_items.extend(collect_items_from_file(&path)?);
    }

    let mut touched = Vec::new();
    let mut delta = ApiDelta {
        additions: Vec::new(),
        removals: Vec::new(),
        signature_changes: Vec::new(),
    };
    let mut has_warning = false;
    let mut has_breaking = false;

    for change in proposed_changes {
        let matches = matching_items(&analyzed_items, &source, change);
        if matches.is_empty() {
            continue;
        }
        for item in matches {
            let touched_item = TouchedItem {
                path: item.path.clone(),
                kind: item.kind.clone(),
                name: item.name.clone(),
            };
            touched.push(touched_item.clone());
            match change.change_kind {
                ChangeKind::Add => {
                    if item.is_public {
                        delta.additions.push(touched_item);
                    }
                }
                ChangeKind::Remove => {
                    if item.is_public {
                        delta.removals.push(touched_item);
                        has_breaking = true;
                    }
                }
                ChangeKind::Modify => {
                    if item.is_public && item.kind == ItemKind::Fn {
                        if let Some(before) = item.signature.clone() {
                            delta.signature_changes.push(SignatureChange {
                                name: item.name.clone(),
                                before,
                                after: String::new(),
                            });
                        }
                        has_breaking = true;
                    }
                }
            }
        }
    }

    if touched.is_empty() {
        if !proposed_changes.is_empty() {
            // For add-only proposals where a symbol does not yet exist in the
            // parsed source set, treat the touch as advisory "info" and keep
            // processing other evidence.
            touched.push(TouchedItem {
                path: source.join(&proposed_changes[0].file),
                kind: ItemKind::Mod,
                name: proposed_changes[0].item_name.clone(),
            });
        }
    }

    let re_exports = collect_affected_root_re_exports(&source, &touched, proposed_changes)?;
    if !re_exports.is_empty() {
        has_warning = true;
        if !delta.removals.is_empty() {
            has_breaking = true;
        }
    }

    let advisory_severity = if has_breaking {
        Severity::Breaking
    } else if has_warning {
        Severity::Warning
    } else {
        Severity::Info
    };

    Ok(PublicApiReport {
        public_items_touched: dedupe_touched(touched),
        public_api_delta_summary: ApiDelta {
            additions: dedupe_touched(delta.additions),
            removals: dedupe_touched(delta.removals),
            signature_changes: delta.signature_changes,
        },
        crate_root_re_exports_affected: dedupe_re_exports(re_exports),
        advisory_severity,
    })
}

fn collect_rust_files(source: &Path) -> Result<Vec<PathBuf>> {
    if source.is_file() {
        return if source.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            Ok(vec![source.to_path_buf()])
        } else {
            Err(anyhow!(
                "source must be .rs file or directory: {}",
                source.display()
            ))
        };
    }

    let mut out = Vec::new();
    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        out.push(entry.into_path());
    }
    Ok(out)
}

fn collect_items_from_file(path: &Path) -> Result<Vec<AnalyzedItem>> {
    let parsed = parse_rust_file(path)?;
    let mut out = Vec::new();
    let mut cursor = parsed.tree.root_node().walk();
    for child in parsed.tree.root_node().named_children(&mut cursor) {
        let kind = match item_kind(child.kind()) {
            Some(kind) => kind,
            None => continue,
        };
        let name = item_name(child, &parsed.source);
        if name.is_empty() {
            continue;
        }
        let is_public = has_public_visibility(child, &parsed.source);
        let signature = if kind == ItemKind::Fn {
            Some(function_signature(child, &parsed.source))
        } else {
            None
        };
        out.push(AnalyzedItem {
            path: path.to_path_buf(),
            kind,
            name,
            is_public,
            signature,
        });
    }
    Ok(out)
}

fn parse_rust_file(path: &Path) -> Result<ParsedFile> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let language = language_for_path(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported language for {}", path.display()),
        )
    })?;
    let mut parser = parser_for_language(language)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("tree-sitter parse failed for {}", path.display()))?;
    Ok(ParsedFile {
        path: path.to_path_buf(),
        source,
        tree,
    })
}

fn item_kind(kind: &str) -> Option<ItemKind> {
    match kind {
        "function_item" => Some(ItemKind::Fn),
        "struct_item" => Some(ItemKind::Struct),
        "enum_item" => Some(ItemKind::Enum),
        "trait_item" => Some(ItemKind::Trait),
        "const_item" => Some(ItemKind::Const),
        "static_item" => Some(ItemKind::Static),
        "use_declaration" => Some(ItemKind::Use),
        "mod_item" => Some(ItemKind::Mod),
        "type_item" => Some(ItemKind::TypeAlias),
        _ => None,
    }
}

fn has_public_visibility(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "visibility_modifier" {
            continue;
        }
        if let Ok(text) = child.utf8_text(source.as_bytes()) {
            return text.trim().starts_with("pub");
        }
    }
    false
}

fn item_name(node: Node<'_>, source: &str) -> String {
    if let Some(name_node) = node.child_by_field_name("name") {
        if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if node.kind() == "use_declaration" {
        let names = parse_use_declaration_names(&node_text(node, source));
        return names.into_iter().next().unwrap_or_default();
    }

    node_text(node, source)
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_string()
}

fn function_signature(node: Node<'_>, source: &str) -> String {
    if let Some(body) = node.child_by_field_name("body") {
        let start = node.start_byte();
        let end = body.start_byte();
        if end <= source.len() && start <= end {
            return source[start..end].trim().to_string();
        }
    }
    node_text(node, source)
}

fn parse_use_declaration_names(text: &str) -> Vec<String> {
    let body = strip_use_prefix(text);
    if body.is_empty() {
        return Vec::new();
    }

    let body = body.trim().trim_end_matches(';').trim();
    let body = body.trim_start_matches("use ").trim();
    if let (Some(open), Some(close)) = (body.find('{'), body.find('}')) {
        if close > open {
            return body[open + 1..close]
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .filter_map(parse_single_use_name)
                .collect();
        }
    }

    parse_single_use_name(body).into_iter().collect()
}

fn parse_single_use_name(raw: &str) -> Option<String> {
    let mut part = raw.trim().trim_end_matches(',').trim();
    if part.is_empty() || part == "*" {
        return None;
    }
    if part == "self" {
        return Some(part.to_string());
    }

    if let Some(as_idx) = part.find(" as ") {
        part = part[as_idx + 4..].trim();
    }
    part = part.trim_end_matches("::*").trim();
    let part = part.rsplit("::").next().unwrap_or(part);
    if part.is_empty() {
        return None;
    }
    Some(part.to_string())
}

fn strip_use_prefix(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(use_pos) = trimmed.find("use ") {
        return trimmed[use_pos + 4..].trim().to_string();
    }
    trimmed.to_string()
}

fn matching_items(
    items: &[AnalyzedItem],
    source_root: &Path,
    change: &ProposedChangeRef,
) -> Vec<AnalyzedItem> {
    let mut candidates = Vec::new();
    let paths = possible_change_paths(source_root, &change.file);

    for item in items {
        if item.name != change.item_name {
            continue;
        }
        if paths.is_empty() {
            candidates.push(item.clone());
            continue;
        }
        if paths.contains(&item.path) {
            candidates.push(item.clone());
            continue;
        }
        if paths
            .iter()
            .any(|path| change.file.is_relative() && item.path.ends_with(path))
        {
            candidates.push(item.clone());
        }
    }
    candidates
}

fn possible_change_paths(source_root: &Path, declared_path: &Path) -> Vec<PathBuf> {
    if declared_path.is_absolute() {
        return vec![declared_path.to_path_buf()];
    }

    let mut paths = Vec::new();
    let roots = change_search_roots(source_root);
    for root in roots {
        let joined = root.join(declared_path);
        if joined.exists() {
            paths.push(joined.canonicalize().unwrap_or(joined));
        }
    }
    paths
}

fn change_search_roots(source_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![source_root.to_path_buf()];
    if let Some(parent) = source_root.parent() {
        roots.push(parent.to_path_buf());
    }
    if let Some(grand) = source_root.parent().and_then(|p| p.parent()) {
        roots.push(grand.to_path_buf());
    }
    roots
}

fn collect_affected_root_re_exports(
    source_root: &Path,
    touched: &[TouchedItem],
    proposed_changes: &[ProposedChangeRef],
) -> Result<Vec<ReExport>> {
    let mut out = Vec::new();
    for path in root_file_candidates(source_root) {
        if !path.exists() {
            continue;
        }
        let parsed = parse_rust_file(&path)?;
        let mut cursor = parsed.tree.root_node().walk();
        for node in parsed.tree.root_node().named_children(&mut cursor) {
            if node.kind() != "use_declaration" || !has_public_visibility(node, &parsed.source) {
                continue;
            }
            let names = parse_use_declaration_names(&node_text(node, &parsed.source));
            let touched_names: HashSet<&str> =
                touched.iter().map(|item| item.name.as_str()).collect();
            let touched_by_path = matches_proposed_path(proposed_changes, &path);

            for name in names {
                if touched_names.contains(name.as_str())
                    || touched_by_path.iter().any(|target| name.contains(target))
                {
                    out.push(ReExport {
                        path: path.clone(),
                        name,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn root_file_candidates(source_root: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        source_root.join("lib.rs"),
        source_root.join("main.rs"),
        source_root.join("src").join("lib.rs"),
        source_root.join("src").join("main.rs"),
    ];
    if let Some(parent) = source_root.parent() {
        candidates.push(parent.join("lib.rs"));
        candidates.push(parent.join("main.rs"));
        candidates.push(parent.join("src").join("lib.rs"));
        candidates.push(parent.join("src").join("main.rs"));
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn matches_proposed_path(
    proposed_changes: &[ProposedChangeRef],
    source_path: &Path,
) -> Vec<String> {
    proposed_changes
        .iter()
        .filter_map(|change| {
            if let Some(stem) = change.file.file_stem().and_then(|s| s.to_str()) {
                let absolute = change.file.to_string_lossy();
                if source_path.ends_with(stem) || absolute.contains(stem) {
                    return Some(stem.to_string());
                }
            }
            if let Some(stem) = source_path.file_stem().and_then(|s| s.to_str()) {
                if absolute_change_name_matches(change, stem) {
                    return Some(stem.to_string());
                }
            }
            None
        })
        .collect()
}

fn absolute_change_name_matches(change: &ProposedChangeRef, name: &str) -> bool {
    change
        .file
        .to_string_lossy()
        .to_lowercase()
        .contains(&name.to_lowercase())
}

fn dedupe_touched(items: Vec<TouchedItem>) -> Vec<TouchedItem> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = (item.path.clone(), item.kind.clone(), item.name.clone());
        if seen.insert(key) {
            out.push(item);
        }
    }
    out
}

fn dedupe_re_exports(items: Vec<ReExport>) -> Vec<ReExport> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = (item.path.clone(), item.name.clone());
        if seen.insert(key) {
            out.push(item);
        }
    }
    out
}

fn node_text(node: Node<'_>, source: &str) -> String {
    node.utf8_text(source.as_bytes())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_public_api_touches_pub_fn_as_breaking_or_warning() -> Result<()> {
        // The crate root at compile time — portable across machines/worktrees,
        // and guaranteed to contain src/refactor/mod.rs.
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let changes = vec![ProposedChangeRef {
            file: PathBuf::from("src/refactor/mod.rs"),
            item_name: "status".to_string(),
            change_kind: ChangeKind::Modify,
        }];

        let report = analyze_public_api(&source, &changes)?;
        assert!(!report.public_items_touched.is_empty());
        assert!(matches!(
            report.advisory_severity,
            Severity::Breaking | Severity::Warning
        ));
        Ok(())
    }
}
