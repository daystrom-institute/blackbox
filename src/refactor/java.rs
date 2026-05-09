use super::*;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(crate) struct JavaMethod {
    parent_name: String,
    parent_byte_start: usize,
    item: SyntaxItem,
}

#[derive(Debug, Clone)]
pub(crate) struct JavaNestedClass {
    parent_name: String,
    parent_byte_start: usize,
    item: SyntaxItem,
}

pub(crate) fn java_methods(parsed: &ParsedSource) -> Vec<JavaMethod> {
    let mut methods = Vec::new();
    let root = parsed.tree.root_node();
    walk_java_methods(parsed, root, "(root)", 0, &mut methods);
    methods
}

fn walk_java_methods(parsed: &ParsedSource, node: Node<'_>, parent_name: &str, parent_byte_start: usize, methods: &mut Vec<JavaMethod>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration" || kind == "interface_declaration" || kind == "record_declaration" || kind == "enum_declaration" {
            let name = item_name(child, &parsed.source, parsed.language).unwrap_or_else(|| "(unnamed)".to_string());
            if let Some(body) = child.child_by_field_name("body") {
                walk_java_methods(parsed, body, &name, child.start_byte(), methods);
            } else if let Some(body) = child.child_by_field_name("interfaces") {
                 // Enum constants can also have bodies but usually main methods are in class_body
                 walk_java_methods(parsed, child, &name, child.start_byte(), methods);
            } else {
                 walk_java_methods(parsed, child, &name, child.start_byte(), methods);
            }
        } else if kind == "class_body" || kind == "enum_body" || kind == "record_body" {
             walk_java_methods(parsed, child, parent_name, parent_byte_start, methods);
        } else if kind == "method_declaration" || kind == "constructor_declaration" {
            methods.push(JavaMethod {
                parent_name: parent_name.to_string(),
                parent_byte_start,
                item: syntax_item_with_kind(parsed, child, kind),
            });
        } else {
             // Continue walking to find anonymous classes or other nested structures
             walk_java_methods(parsed, child, parent_name, parent_byte_start, methods);
        }
    }
}

pub(crate) fn java_nested_classes(parsed: &ParsedSource) -> Vec<JavaNestedClass> {
    let mut classes = Vec::new();
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration" || kind == "interface_declaration" || kind == "record_declaration" || kind == "enum_declaration" {
             let name = item_name(child, &parsed.source, parsed.language).unwrap_or_else(|| "(unnamed)".to_string());
             walk_java_nested_classes(parsed, child, &name, child.start_byte(), &mut classes);
        }
    }
    classes
}

fn walk_java_nested_classes(parsed: &ParsedSource, node: Node<'_>, parent_name: &str, parent_byte_start: usize, classes: &mut Vec<JavaNestedClass>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration" || kind == "interface_declaration" || kind == "record_declaration" || kind == "enum_declaration" {
            classes.push(JavaNestedClass {
                parent_name: parent_name.to_string(),
                parent_byte_start,
                item: syntax_item_with_kind(parsed, child, kind),
            });
            let name = item_name(child, &parsed.source, parsed.language).unwrap_or_else(|| "(unnamed)".to_string());
            walk_java_nested_classes(parsed, child, &name, child.start_byte(), classes);
        } else if kind == "class_body" || kind == "enum_body" || kind == "record_body" {
            walk_java_nested_classes(parsed, child, parent_name, parent_byte_start, classes);
        }
    }
}

pub(crate) fn plan_extract_java_methods(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_methods"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_methods only supports java files");
    }

    let candidates = java_methods(&parsed);
    if candidates.is_empty() {
        bail!("no Java methods found");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    if names.is_empty() {
        bail!("item_names (method names) must be provided for extract_java_methods");
    }

    let mut selected: Vec<JavaMethod> = Vec::new();
    for expected in names {
        let matches = candidates
            .iter()
            .filter(|m| m.item.name.as_deref() == Some(expected))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested method `{expected}` was not found"),
            [method] => selected.push((**method).clone()),
            _ => bail!(
                "requested method `{expected}` matched multiple methods; method overloading requires more specific targeting (not yet implemented)"
            ),
        }
    }

    // Sort by byte offset descending to safely remove from the bottom up
    selected.sort_by_key(|m| std::cmp::Reverse(m.item.byte_start));

    let mut source_edits = Vec::new();
    let mut extracted_content = Vec::new();

    for method in &selected {
        source_edits.push(TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.byte_end,
            replacement: String::new(),
        });
        let content = &parsed.source[method.item.leading_trivia_start..method.item.byte_end];
        extracted_content.push(content.to_string());
    }

    // extracted_content is collected bottom-up; reverse it for the target file
    extracted_content.reverse();
    
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    
    let target_content = if target_path.exists() {
        let mut text = String::from_utf8(original_target_bytes.clone()).unwrap_or_default();
        let insert_at = text.rfind('}').unwrap_or(text.len());
        text.insert_str(insert_at, &format!("\n{}\n", extracted_content.join("\n\n")));
        text
    } else {
        bail!("target file must exist for extract_java_methods (class wrapper required)");
    };

    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
    };

    let plan = RefactorPlan {
        title: format!("Extract {} methods to {}", selected.len(), target_path.display()),
        kind: "extract_java_methods".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            target_edit,
        ],
        validations: vec![],
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_java_nested_classes(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_nested_classes"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_nested_classes only supports java files");
    }

    let candidates = java_nested_classes(&parsed);
    if candidates.is_empty() {
        bail!("no Java nested classes found");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    if names.is_empty() {
        bail!("item_names (class names) must be provided for extract_java_nested_classes");
    }

    let mut selected: Vec<JavaNestedClass> = Vec::new();
    for expected in names {
        let matches = candidates
            .iter()
            .filter(|c| c.item.name.as_deref() == Some(expected))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested nested class `{expected}` was not found"),
            [class_item] => selected.push((**class_item).clone()),
            _ => bail!("requested nested class `{expected}` matched multiple classes"),
        }
    }

    selected.sort_by_key(|c| std::cmp::Reverse(c.item.byte_start));

    let mut source_edits = Vec::new();
    let mut extracted_content = Vec::new();

    for class_item in &selected {
        source_edits.push(TextEdit {
            byte_start: class_item.item.leading_trivia_start,
            byte_end: class_item.item.byte_end,
            replacement: String::new(),
        });
        let content = &parsed.source[class_item.item.leading_trivia_start..class_item.item.byte_end];
        extracted_content.push(content.to_string());
    }

    extracted_content.reverse();
    
    let prelude = p.target_prelude.clone().unwrap_or_default();
    let target_content = format!("{}\n\n{}\n", prelude, extracted_content.join("\n\n"));

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
    };

    let plan = RefactorPlan {
        title: format!("Extract {} nested classes to {}", selected.len(), target_path.display()),
        kind: "extract_java_nested_classes".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            target_edit,
        ],
        validations: vec![],
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_rewrite_java_visibility(p: &RefactorPlanParams) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "status": "not_implemented",
        "message": "plan_rewrite_java_visibility is currently a stub"
    }))?)
}

use lsp_types::{
    ClientCapabilities, CodeActionClientCapabilities, CodeActionContext, CodeActionKind,
    CodeActionKindLiteralSupport, CodeActionLiteralSupport, CodeActionParams, DocumentChanges,
    InitializeParams, Position, Range, RenameParams, ResourceOperationKind,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkspaceClientCapabilities, WorkspaceEdit, WorkspaceEditClientCapabilities, WorkspaceFolder,
    request::{CodeActionRequest, Initialize, Rename, Request, Shutdown},
    notification::{Exit, Initialized, Notification},
};

pub(crate) fn jdtls_organize_imports(
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<FileEdit>> {
    let mut child = std::process::Command::new("jdtls")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning jdtls")?;
    let mut stdin = child.stdin.take().context("jdtls stdin")?;
    let stdout = child.stdout.take().context("jdtls stdout")?;
    let mut reader = std::io::BufReader::new(stdout);
    
    let root_uri = Url::from_directory_path(project_dir)
        .map_err(|_| anyhow!("failed to convert {} to file URL", project_dir.display()))?;
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
        
    let init_params = InitializeParams {
        process_id: Some(std::process::id()),
        root_uri: Some(root_uri.clone()),
        root_path: Some(project_dir.to_string_lossy().to_string()),
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                code_action: Some(CodeActionClientCapabilities {
                    code_action_literal_support: Some(CodeActionLiteralSupport {
                        code_action_kind: CodeActionKindLiteralSupport {
                            value_set: vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS.as_str().to_string()],
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: root_uri,
            name: "refactor-root".to_string(),
        }]),
        ..Default::default()
    };

    send_lsp_request::<Initialize>(&mut stdin, 1, &init_params)?;
    let _init_result = read_lsp_response::<Initialize>(&mut reader, 1)?;
    send_lsp_notification::<Initialized>(&mut stdin, &lsp_types::InitializedParams {})?;
    
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let end_position = byte_to_lsp_position(&source, source.len());
    
    std::thread::sleep(std::time::Duration::from_millis(5000)); // jdtls can be slow
    
    let code_action_params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        range: Range {
            start: Position { line: 0, character: 0 },
            end: end_position,
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    
    send_lsp_request::<CodeActionRequest>(&mut stdin, 2, &code_action_params)?;
    let response = read_lsp_response::<CodeActionRequest>(&mut reader, 2)?;
    
    let _ = send_lsp_request::<Shutdown>(&mut stdin, 3, &());
    let _ = send_lsp_notification::<Exit>(&mut stdin, &());
    let _ = child.wait();
    
    let mut all_edits = Vec::new();
    if let Some(actions) = response {
        for action in actions {
            match action {
                lsp_types::CodeActionOrCommand::CodeAction(ca) => {
                    let kind = ca.kind.clone().unwrap_or_else(|| lsp_types::CodeActionKind::from(""));
                    if kind == CodeActionKind::SOURCE_ORGANIZE_IMPORTS || ca.title.to_ascii_lowercase().contains("organize") {
                        if let Some(edit) = ca.edit {
                            all_edits.extend(workspace_edit_to_file_edits(edit)?);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(all_edits)
}

pub(crate) fn plan_java_lsp_organize_imports(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let project_dir_str = project_dir.to_string_lossy();
    let source_path = resolve_path(Some(&project_dir_str), &p.source)?;

    let file_edits = jdtls_organize_imports(&project_dir, &source_path)?;
    if file_edits.is_empty() {
        bail!("jdtls returned no import organization edits");
    }

    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!("Organize Java imports in {}", p.source),
        kind: "java_lsp_organize_imports".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: false,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use crate::refactor::{RefactorPlanParams, java::plan_extract_java_methods};

    #[test]
    fn test_extract_methods() {
        let source_code = "
public class GodClass {
    public void methodA() { System.out.println(\"A\"); }
    public void methodB() { System.out.println(\"B\"); }
}
";
        std::fs::write("/tmp/GodClass.java", source_code).unwrap();
        std::fs::write("/tmp/TargetClass.java", "public class TargetClass {\n}\n").unwrap();
        
        let p = RefactorPlanParams {
            kind: "extract_java_methods".to_string(),
            source: "GodClass.java".to_string(),
            target: Some("TargetClass.java".to_string()),
            project_dir: Some("/tmp".to_string()),
            item_names: Some(vec!["methodA".to_string()]),
            item_kinds: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            module_name: None,
            router_name: None,
            router_export_name: None,
            router_call: None,
            impl_name: None,
            use_path: None,
            visibility: None,
            toml_table: None,
            toml_entries: None,
        };
        
        let plan = plan_extract_java_methods(&p).unwrap();
        assert!(plan.contains("methodA"));
        println!("{}", plan);
    }
}
