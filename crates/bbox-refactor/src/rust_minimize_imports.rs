//! `rust_minimize_imports` plan kind.
//!
//! Conservative wildcard-import minimization for Rust modules. The planner only
//! rewrites wildcard imports whose source module can be resolved to a local Rust
//! file and whose imported names are directly referenced in the source file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tree_sitter::Node;

use super::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, parse_rust_file, path_string, resolve_path, rust_items, sha256_hex,
    validate_plan_shape,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct WildcardUse {
    byte_start: usize,
    byte_end: usize,
    visibility_prefix: String,
    base_path: String,
    text: String,
}

pub(crate) fn plan_minimize_imports(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_rust_file(&source_path)?;
    let allowlist = toml_str_set(&p.toml_entries, "allow_wildcards");
    let remove_unused = toml_bool(&p.toml_entries, "remove_unused_wildcards");

    let local_names = rust_items(&parsed)
        .into_iter()
        .filter(|item| item.kind != "use_declaration")
        .filter_map(|item| item.name)
        .collect::<BTreeSet<_>>();
    let referenced = collect_referenced_identifiers(parsed.tree.root_node(), &parsed.source);
    let wildcards = collect_wildcard_uses(parsed.tree.root_node(), &parsed.source);

    let mut edits = Vec::new();
    let mut leftovers = Vec::new();
    for wildcard in wildcards {
        if wildcard_is_allowed(&allowlist, &wildcard.base_path) {
            leftovers.push(format!(
                "preserved allowlisted wildcard import `{}`",
                wildcard.text.trim()
            ));
            continue;
        }

        let target_path = match resolve_wildcard_source(
            &source_path,
            p.project_dir.as_deref(),
            &wildcard.base_path,
        ) {
            Ok(path) => path,
            Err(err) => {
                leftovers.push(format!(
                    "could not resolve wildcard import `{}`: {err:#}",
                    wildcard.text.trim()
                ));
                continue;
            }
        };
        let target = parse_rust_file(&target_path)
            .with_context(|| format!("parsing wildcard source {}", target_path.display()))?;
        let importable_names = rust_items(&target)
            .into_iter()
            .filter(|item| item.kind != "use_declaration" && item.kind != "mod_item")
            .filter_map(|item| item.name)
            .filter(|name| !local_names.contains(name))
            .collect::<BTreeSet<_>>();
        let used_names = importable_names
            .intersection(&referenced)
            .cloned()
            .collect::<Vec<_>>();

        if used_names.is_empty() {
            if remove_unused {
                edits.push(TextEdit {
                    byte_start: line_start(&parsed.source, wildcard.byte_start),
                    byte_end: line_end_including_newline(&parsed.source, wildcard.byte_end),
                    replacement: String::new(),
                });
            } else {
                leftovers.push(format!(
                    "left wildcard import `{}` unchanged: no directly referenced names found; pass toml_entries.remove_unused_wildcards=true to delete it",
                    wildcard.text.trim()
                ));
            }
            continue;
        }

        let replacement = format!(
            "{}use {}::{{{}}};",
            wildcard.visibility_prefix,
            wildcard.base_path,
            used_names.join(", ")
        );
        edits.push(TextEdit {
            byte_start: wildcard.byte_start,
            byte_end: wildcard.byte_end,
            replacement,
        });
    }

    if edits.is_empty() {
        if leftovers.is_empty() {
            bail!("no wildcard imports found in {}", source_path.display());
        }
        bail!(
            "no wildcard imports could be minimized in {}: {}",
            source_path.display(),
            leftovers.join("; ")
        );
    }

    let plan = RefactorPlan {
        title: format!(
            "minimize Rust wildcard imports in {}",
            path_string(&source_path)
        ),
        kind: "rust_minimize_imports".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
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
        operator_opt_outs_used: Vec::new(),
    };
    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn collect_wildcard_uses(root: Node<'_>, source: &str) -> Vec<WildcardUse> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" {
            if let Some(wildcard) = parse_wildcard_use(node, source) {
                out.push(wildcard);
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_by_key(|wildcard| wildcard.byte_start);
    out
}

fn parse_wildcard_use(node: Node<'_>, source: &str) -> Option<WildcardUse> {
    let text = source
        .get(node.start_byte()..node.end_byte())?
        .trim()
        .to_string();
    let without_semicolon = text.strip_suffix(';')?.trim();
    let use_idx = without_semicolon.find("use ")?;
    let visibility_prefix = without_semicolon[..use_idx].to_string();
    let use_path = without_semicolon[use_idx + "use ".len()..].trim();
    let base_path = use_path.strip_suffix("::*")?.trim();
    if base_path.is_empty() || base_path.contains('{') || base_path.contains('}') {
        return None;
    }
    Some(WildcardUse {
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        visibility_prefix,
        base_path: base_path.to_string(),
        text,
    })
}

fn collect_referenced_identifiers(root: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "type_identifier" | "scoped_identifier" => {
                if let Some(text) = source.get(node.start_byte()..node.end_byte()) {
                    if is_plain_identifier(text) {
                        out.insert(text.to_string());
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

fn resolve_wildcard_source(
    source_path: &Path,
    project_dir: Option<&str>,
    base_path: &str,
) -> Result<PathBuf> {
    let segments = base_path.split("::").collect::<Vec<_>>();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        bail!("wildcard import path must contain non-empty segments");
    }

    match segments[0] {
        "self" => resolve_module_child_file(&module_child_dir(source_path), &segments[1..]),
        "super" => {
            let parent_file = parent_module_file(source_path)
                .ok_or_else(|| anyhow!("could not locate parent module file"))?;
            if segments.len() == 1 {
                Ok(parent_file)
            } else {
                resolve_module_child_file(&module_child_dir(&parent_file), &segments[1..])
            }
        }
        "crate" => {
            let project_dir = project_dir
                .map(PathBuf::from)
                .or_else(|| find_project_dir_from_source(source_path))
                .ok_or_else(|| anyhow!("project_dir is required to resolve crate::* imports"))?;
            let src_dir = project_dir.join("src");
            if segments.len() == 1 {
                for candidate in [src_dir.join("lib.rs"), src_dir.join("main.rs")] {
                    if candidate.exists() {
                        return Ok(candidate);
                    }
                }
                bail!("could not locate crate root at src/lib.rs or src/main.rs");
            }
            resolve_module_child_file(&src_dir, &segments[1..])
        }
        _ => resolve_module_child_file(&module_child_dir(source_path), &segments),
    }
}

fn resolve_module_child_file(base_dir: &Path, segments: &[&str]) -> Result<PathBuf> {
    if segments.is_empty() {
        bail!("module child path must not be empty");
    }
    let mut dir = base_dir.to_path_buf();
    for segment in &segments[..segments.len() - 1] {
        dir.push(segment);
    }
    let last = segments[segments.len() - 1];
    let candidates = [
        dir.join(format!("{last}.rs")),
        dir.join(last).join("mod.rs"),
    ];
    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            anyhow!(
                "could not locate module file for `{}` under {}",
                segments.join("::"),
                base_dir.display()
            )
        })
}

fn parent_module_file(source_path: &Path) -> Option<PathBuf> {
    let file_name = source_path.file_name()?.to_str()?;
    if file_name == "mod.rs" {
        let current_dir = source_path.parent()?;
        let parent_dir = current_dir.parent()?;
        let module_name = current_dir.file_name()?.to_str()?;
        for candidate in [
            parent_dir.join(format!("{module_name}.rs")),
            parent_dir.join(module_name).join("mod.rs"),
        ] {
            if candidate.exists() && candidate != source_path {
                return Some(candidate);
            }
        }
        return None;
    }

    let parent_dir = source_path.parent()?;
    let module_name = parent_dir.file_name()?.to_str()?;
    let grandparent = parent_dir.parent()?;
    [
        grandparent.join(format!("{module_name}.rs")),
        parent_dir.join("mod.rs"),
    ]
    .into_iter()
    .find(|candidate| candidate.exists() && candidate != source_path)
}

fn module_child_dir(source_path: &Path) -> PathBuf {
    if source_path.file_name().and_then(|name| name.to_str()) == Some("mod.rs") {
        source_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf()
    } else {
        source_path.with_extension("")
    }
}

fn find_project_dir_from_source(source_path: &Path) -> Option<PathBuf> {
    let mut dir = source_path.parent()?;
    loop {
        if dir.file_name().and_then(|name| name.to_str()) == Some("src") {
            return dir.parent().map(Path::to_path_buf);
        }
        dir = dir.parent()?;
    }
}

fn wildcard_is_allowed(allowlist: &BTreeSet<String>, base_path: &str) -> bool {
    allowlist.contains(base_path) || allowlist.contains(&format!("{base_path}::*"))
}

fn toml_str_set(
    entries: &Option<BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> BTreeSet<String> {
    entries
        .as_ref()
        .and_then(|entries| entries.get(key))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect()
}

fn toml_bool(entries: &Option<BTreeMap<String, serde_json::Value>>, key: &str) -> bool {
    entries
        .as_ref()
        .and_then(|entries| entries.get(key))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn is_plain_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn line_start(source: &str, byte: usize) -> usize {
    source[..byte].rfind('\n').map_or(0, |idx| idx + 1)
}

fn line_end_including_newline(source: &str, byte: usize) -> usize {
    source[byte..]
        .find('\n')
        .map_or(source.len(), |idx| byte + idx + 1)
}
