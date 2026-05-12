use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Location, Position, TextDocumentIdentifier,
    TextDocumentPositionParams, Url, request::GotoDefinition,
};
use serde::Serialize;
use tree_sitter::Node;

use super::*;
use crate::lsp::LspError;
use crate::projects::Language;

#[derive(Debug, Serialize)]
struct PlanWithResolvedCallbacks {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resolved_callbacks: Vec<ResolvedCallback>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedCallback {
    pub method: String,
    pub declaring_item: String,
    pub declaring_kind: String,
    pub call_sites: Vec<ExtractedCallSite>,
}

pub fn plan_ra_classify(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    let project_dir_str = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required"))?;
    let project_dir = Path::new(project_dir_str);
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let item_names = p
        .item_names
        .as_ref()
        .ok_or_else(|| anyhow!("item_names are required"))?;

    let manager = ctx
        .lsp
        .as_ref()
        .ok_or_else(|| anyhow!("error.lsp_unavailable: LSP session manager missing"))?;

    let source_text = fs::read_to_string(&source_path)
        .with_context(|| format!("reading source file {}", source_path.display()))?;
    let parsed = parse_rust_file(&source_path)?;

    let call_sites = find_call_sites(&parsed, item_names)?;
    if call_sites.is_empty() {
        let mut plan = empty_plan("Rust RA Classify Callbacks", "rust_ra_classify_callbacks");
        plan.semantic_status = SemanticStatus::LspVerified;
        return Ok(serde_json::to_string_pretty(&plan)?);
    }

    let source_uri = Url::from_file_path(&source_path)
        .map_err(|_| anyhow!("failed to convert {} to URL", source_path.display()))?;

    let resolved_callbacks = manager.with_session(project_dir, Language::Rust, |mut client| {
        // Open the document and wait for diagnostics to ensure index is warm
        client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
            &lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: source_uri.clone(),
                    language_id: "rust".to_string(),
                    version: 0,
                    text: source_text.clone(),
                },
            },
        )?;
        client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(30));

        let mut by_method: std::collections::HashMap<
            (String, String, String),
            Vec<ExtractedCallSite>,
        > = std::collections::HashMap::new();

        for (callee, site, pos) in call_sites {
            let def_params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let id = client.send_request::<GotoDefinition>(&def_params)?;
            let response = client.read_response::<GotoDefinition>(id)?;

            let location = match response {
                Some(GotoDefinitionResponse::Scalar(loc)) => Some(loc),
                Some(GotoDefinitionResponse::Array(mut locs)) if !locs.is_empty() => {
                    Some(locs.remove(0))
                }
                _ => None,
            };

            if let Some(loc) = location {
                let (decl_item, decl_kind) =
                    classify_definition(project_dir, &loc).map_err(|e| LspError::Other(e))?;
                by_method
                    .entry((callee, decl_item, decl_kind))
                    .or_default()
                    .push(site);
            } else {
                // If no definition found, treat as external or unknown callback
                by_method
                    .entry((callee, "unknown".to_string(), "external".to_string()))
                    .or_default()
                    .push(site);
            }
        }

        let mut results = Vec::new();
        for ((method, declaring_item, declaring_kind), sites) in by_method {
            results.push(ResolvedCallback {
                method,
                declaring_item,
                declaring_kind,
                call_sites: sites,
            });
        }
        results.sort_by(|a, b| a.method.cmp(&b.method));

        Ok(results)
    })?;

    let mut plan = empty_plan("Rust RA Classify Callbacks", "rust_ra_classify_callbacks");
    plan.semantic_status = SemanticStatus::LspVerified;

    let response = PlanWithResolvedCallbacks {
        plan,
        resolved_callbacks,
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

fn empty_plan(title: &str, kind: &str) -> RefactorPlan {
    RefactorPlan {
        title: title.to_string(),
        kind: kind.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: Vec::new(),
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    }
}

fn find_call_sites(
    parsed: &ParsedSource,
    item_names: &[String],
) -> Result<Vec<(String, ExtractedCallSite, Position)>> {
    let mut sites = Vec::new();
    let methods = rust_impl_methods(parsed);
    let name_set: HashSet<_> = item_names.iter().map(|s| s.as_str()).collect();

    let root = parsed.tree.root_node();

    for method in methods {
        let name = method.item.name.as_deref().unwrap_or("");
        if !name_set.contains(name) {
            continue;
        }

        let Some(fn_node) = rust_node_by_range(
            root,
            "function_item",
            method.item.byte_start,
            method.item.byte_end,
        ) else {
            continue;
        };

        walk_for_ra_callbacks(&parsed.source, fn_node, name, &mut sites);
    }
    Ok(sites)
}

fn walk_for_ra_callbacks(
    source: &str,
    node: Node<'_>,
    in_method: &str,
    sites: &mut Vec<(String, ExtractedCallSite, Position)>,
) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            let mut callee = None;
            let mut name_node = None;

            match func.kind() {
                "field_expression" => {
                    if let Some(value) = func.child_by_field_name("value") {
                        if value.utf8_text(source.as_bytes()).ok() == Some("self") {
                            if let Some(field) = func.child_by_field_name("field") {
                                callee = Some(format!(
                                    "self.{}",
                                    field.utf8_text(source.as_bytes()).unwrap_or("")
                                ));
                                name_node = Some(field);
                            }
                        }
                    }
                }
                "scoped_identifier" => {
                    if let (Some(path), Some(name)) = (
                        func.child_by_field_name("path"),
                        func.child_by_field_name("name"),
                    ) {
                        if path.utf8_text(source.as_bytes()).ok() == Some("Self") {
                            callee = Some(format!(
                                "Self::{}",
                                name.utf8_text(source.as_bytes()).unwrap_or("")
                            ));
                            name_node = Some(name);
                        }
                    }
                }
                _ => {}
            }

            if let (Some(callee_str), Some(node)) = (callee, name_node) {
                let start_byte = node.start_byte();
                let pos = byte_to_lsp_position(source, start_byte);

                let mut context = "direct".to_string();
                let mut parent = node.parent();
                while let Some(p) = parent {
                    if p.kind() == "function_item" {
                        break;
                    }
                    if p.kind() == "lambda_expression" {
                        context = "lambda".to_string();
                        break;
                    }
                    parent = p.parent();
                }

                let (line, column) = line_col(source, start_byte);
                sites.push((
                    callee_str,
                    ExtractedCallSite {
                        line,
                        column,
                        in_method: in_method.to_string(),
                        context,
                    },
                    pos,
                ));
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_ra_callbacks(source, child, in_method, sites);
    }
}

fn classify_definition(project_dir: &Path, loc: &Location) -> Result<(String, String)> {
    let path = loc.uri.to_file_path().map_err(|_| anyhow!("invalid URI"))?;
    if !path.starts_with(project_dir) {
        return Ok(("external".to_string(), "external".to_string()));
    }

    let source = fs::read_to_string(&path)
        .with_context(|| format!("reading definition file {}", path.display()))?;
    let parsed = parse_rust_file(&path)?;

    let byte_pos = lsp_position_to_byte(&source, loc.range.start.line, loc.range.start.character)?;
    let root = parsed.tree.root_node();

    let mut node = root.descendant_for_byte_range(byte_pos, byte_pos);
    let mut item_name = "unknown".to_string();
    let mut kind = "external".to_string();

    while let Some(n) = node {
        if n.kind() == "function_item" || n.kind() == "trait_item" || n.kind() == "impl_item" {
            if let Some(name_node) = n.child_by_field_name("name") {
                item_name = name_node
                    .utf8_text(source.as_bytes())
                    .unwrap_or("unknown")
                    .to_string();
            }

            if n.kind() == "function_item" {
                kind = "inherent".to_string(); // Default for internal fns

                let mut parent = n.parent();
                while let Some(p) = parent {
                    if p.kind() == "impl_item" {
                        if p.child_by_field_name("trait").is_some() {
                            kind = "trait_impl".to_string();

                            // Check if it's a blanket impl: impl<T: Trait> Trait for T
                            if let Some(type_node) = p.child_by_field_name("type") {
                                let type_text =
                                    type_node.utf8_text(source.as_bytes()).unwrap_or("");
                                if let Some(params) = p.child_by_field_name("type_parameters") {
                                    let params_text =
                                        params.utf8_text(source.as_bytes()).unwrap_or("");
                                    if params_text.contains(type_text) {
                                        kind = "blanket_impl".to_string();
                                    }
                                }
                            }
                        } else {
                            kind = "inherent".to_string();
                        }
                        break;
                    }
                    if p.kind() == "trait_item" {
                        kind = "trait_impl".to_string();
                        break;
                    }
                    parent = p.parent();
                }
            }
            break;
        }
        node = n.parent();
    }

    Ok((item_name, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn ra_available() -> bool {
        Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn test_find_call_sites() {
        let source = r#"
            impl Foo {
                fn a(&self) {
                    self.b();
                }
                fn b(&self) {
                    Self::c();
                }
                fn c() {}
            }
        "#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(&path, source).unwrap();
        let parsed = parse_rust_file(&path).unwrap();

        let sites = find_call_sites(&parsed, &vec!["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(sites.len(), 2);

        assert_eq!(sites[0].0, "self.b");
        assert_eq!(sites[0].1.in_method, "a");

        assert_eq!(sites[1].0, "Self::c");
        assert_eq!(sites[1].1.in_method, "b");
    }

    #[test]
    #[ignore = "RX-R2 LSP-integration — requires rust-analyzer + cargo project setup, flaky outside CI"]
    fn test_ra_classify_inherent() {
        if !ra_available() {
            return;
        }
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"classify_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let source = dir.path().join("src").join("lib.rs");
        fs::write(
            &source,
            r#"
            pub struct Foo;
            impl Foo {
                pub fn a(&self) {
                    self.b();
                }
                pub fn b(&self) {}
            }
            "#,
        )
        .unwrap();

        let ctx = PlanContext {
            lsp: Some(crate::lsp::LspSessionManager::new()),
        };
        let params = RefactorPlanParams {
            kind: "rust_ra_classify_callbacks".into(),
            source: path_string(&source),
            item_names: Some(vec!["a".into()]),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        };

        let plan_text = plan_ra_classify(&params, &ctx).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(plan_value["semantic_status"], "lsp_verified");
        let callbacks = plan_value["resolved_callbacks"].as_array().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0]["method"], "self.b");
        assert_eq!(callbacks[0]["declaring_item"], "b");
        assert_eq!(callbacks[0]["declaring_kind"], "inherent");
    }

    #[test]
    #[ignore = "RX-R2 LSP-integration"]
    fn test_ra_classify_trait() {
        if !ra_available() {
            return;
        }
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"classify_fixture_trait\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ).unwrap();
        let source = dir.path().join("src").join("lib.rs");
        fs::write(
            &source,
            r#"
            pub trait MyTrait {
                fn trait_fn(&self);
            }
            pub struct Foo;
            impl MyTrait for Foo {
                fn trait_fn(&self) {}
            }
            impl Foo {
                pub fn a(&self) {
                    self.trait_fn();
                }
            }
            "#,
        )
        .unwrap();

        let ctx = PlanContext {
            lsp: Some(crate::lsp::LspSessionManager::new()),
        };
        let params = RefactorPlanParams {
            kind: "rust_ra_classify_callbacks".into(),
            source: path_string(&source),
            item_names: Some(vec!["a".into()]),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        };

        let plan_text = plan_ra_classify(&params, &ctx).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();

        let callbacks = plan_value["resolved_callbacks"].as_array().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0]["method"], "self.trait_fn");
        assert_eq!(callbacks[0]["declaring_kind"], "trait_impl");
    }

    #[test]
    #[ignore = "RX-R2 LSP-integration"]
    fn test_ra_classify_external() {
        if !ra_available() {
            return;
        }
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"classify_fixture_external\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ).unwrap();
        let source = dir.path().join("src").join("lib.rs");
        fs::write(
            &source,
            r#"
            #[derive(Clone)]
            pub struct Foo;
            impl Foo {
                pub fn a(&self) {
                    let _ = self.clone();
                }
            }
            "#,
        )
        .unwrap();

        let ctx = PlanContext {
            lsp: Some(crate::lsp::LspSessionManager::new()),
        };
        let params = RefactorPlanParams {
            kind: "rust_ra_classify_callbacks".into(),
            source: path_string(&source),
            item_names: Some(vec!["a".into()]),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        };

        let plan_text = plan_ra_classify(&params, &ctx).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();

        let callbacks = plan_value["resolved_callbacks"].as_array().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0]["method"], "self.clone");
        assert_eq!(callbacks[0]["declaring_kind"], "external");
    }

    #[test]
    fn test_ra_unavailable() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn a() {}").unwrap();

        let ctx = PlanContext { lsp: None };
        let params = RefactorPlanParams {
            kind: "rust_ra_classify_callbacks".into(),
            source: path_string(&source),
            item_names: Some(vec!["a".into()]),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        };

        let err = plan_ra_classify(&params, &ctx).unwrap_err();
        assert!(err.to_string().contains("error.lsp_unavailable"));
    }
}
