//! RX-R1 — rust-analyzer-backed top-level item move into module.
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    DidOpenTextDocumentParams, Range, TextDocumentIdentifier, TextDocumentItem, Url,
    notification::DidOpenTextDocument, request::CodeActionRequest,
};

use crate::lsp::{LspError, LspSessionManager};
use crate::lsp::convert::{byte_to_lsp_position, workspace_edit_to_file_edits};
use crate::projects::Language;

use super::*;

const MOVE_TO_MODULE_KIND: &str = "refactor.move";
const MOVE_TO_MODULE_TITLE: &str = "move to module";

pub fn plan_ra_move(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for rust_ra_move_item_to_module"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;

    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let item_names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for rust_ra_move_item_to_module"))?;

    let item_kinds = p.item_kinds.as_deref();
    if item_kinds.is_some_and(|kinds| kinds.iter().any(|kind| kind == "impl_method")) {
        bail!(
            "error.bad_input(code=impl_method_unsupported_in_ra_move): impl_method is unsupported in rust_ra_move_item_to_module; use extract_rust_impl_methods"
        );
    }

    let parsed = parse_rust_file(&source_path)?;
    let source_text = parsed.source.clone();
    let source_items: Vec<SyntaxItem> = rust_items(&parsed)
        .into_iter()
        .filter(|item| is_move_target_kind(&item.kind))
        .collect();

    let item_kinds_owned = item_kinds.map(|k| k.to_vec());
    let selected = select_items(&source_items, item_names, item_kinds_owned.as_ref())?;
    if selected.is_empty() {
        bail!("no selectable Rust items found for rust_ra_move_item_to_module");
    }

    let project_dir = p
        .project_dir
        .as_deref()
        .map(|dir| resolve_path(None, dir))
        .transpose()?
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(&source_path)
                .unwrap_or_else(|| source_path.parent().unwrap_or(Path::new(".")).to_path_buf())
        });

    let manager = LspSessionManager::new();
    let file_edits = request_move_actions(
        &manager,
        &project_dir,
        &source_path,
        &source_text,
        &selected,
    )
    .map_err(|err| anyhow!("error.lsp_unavailable: {err}"))?;

    if file_edits.is_empty() {
        bail!("rust-analyzer returned no move-to-module edits");
    }

    let mut validations = Vec::new();
    let mut validation_paths = HashSet::new();
    for edit in &file_edits {
        if validation_paths.insert(edit.path.clone()) {
            validations.extend(parse_validation_step_for_path(Path::new(&edit.path)));
        }
    }

    let plan = RefactorPlan {
        title: format!(
            "move {} Rust item(s) from {} to {}",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "rust_ra_move_item_to_module".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: file_edits,
        validations,
        items: selected.clone(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn is_move_target_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "mod_item"
    )
}

fn select_items(
    source_items: &[SyntaxItem],
    names: &[String],
    item_kinds: Option<&Vec<String>>,
) -> Result<Vec<SyntaxItem>> {
    let mut selected = Vec::new();
    for expected in names {
        let mut matches = source_items
            .iter()
            .filter(|item| item.name.as_deref() == Some(expected.as_str()))
            .filter(|item| match item_kinds {
                Some(kinds) => kinds.iter().any(|kind| kind == &item.kind),
                None => true,
            })
            .filter(|item| is_move_target_kind(&item.kind))
            .cloned()
            .collect::<Vec<_>>();

        if matches.is_empty() {
            bail!("requested Rust item `{expected}` was not found");
        }
        if matches.len() > 1 {
            bail!("requested Rust item `{expected}` matched multiple declarations");
        }
        selected.push(matches.remove(0));
    }
    Ok(selected)
}

fn request_move_actions(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    source_text: &str,
    selected: &[SyntaxItem],
) -> Result<Vec<FileEdit>> {
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;

    let all_edits = manager.with_session(project_dir, Language::Rust, |mut client| {
        client.send_notification::<DidOpenTextDocument>(&DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: source_uri.clone(),
                language_id: "rust".to_string(),
                version: 0,
                text: source_text.to_string(),
            },
        })?;
        client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));

        let mut file_edits = Vec::new();
        for item in selected {
            let item_name = item
                .name
                .clone()
                .unwrap_or_else(|| "<anonymous>".to_string());
            let range = Range {
                start: byte_to_lsp_position(source_text, item.byte_start),
                end: byte_to_lsp_position(source_text, item.byte_end),
            };
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: source_uri.clone(),
                },
                range,
                context: CodeActionContext {
                    diagnostics: vec![],
                    only: Some(vec![CodeActionKind::from(MOVE_TO_MODULE_KIND)]),
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let id = client.send_request::<CodeActionRequest>(&params)?;
            let actions: Option<Vec<lsp_types::CodeActionOrCommand>> =
                client.read_response::<CodeActionRequest>(id)?;
            if actions.is_none() {
                return Err(LspError::Other(anyhow!(
                    "no code actions returned for `{item_name}`"
                )));
            }
            let mut matched = false;
            for action in actions.unwrap() {
                if let CodeActionOrCommand::CodeAction(code_action) = action {
                    let title = code_action.title.to_ascii_lowercase();
                    if !title.contains(MOVE_TO_MODULE_TITLE) {
                        continue;
                    }
                    if let Some(action_kind) = code_action.kind.as_ref() {
                        if action_kind.as_str() != MOVE_TO_MODULE_KIND
                            && !title.contains(MOVE_TO_MODULE_TITLE)
                        {
                            continue;
                        }
                    }

                    let edit = code_action.edit.ok_or_else(|| {
                        LspError::Other(anyhow!(
                            "move-to-module action for `{item_name}` had no edit"
                        ))
                    })?;
                    let edits = workspace_edit_to_file_edits(edit).map_err(LspError::Other)?;
                    file_edits.extend(edits);
                    matched = true;
                }
            }
            if !matched {
                return Err(LspError::Other(anyhow!(
                    "no move-to-module code action found for `{item_name}`"
                )));
            }
        }

        Ok(file_edits)
    })?;

    Ok(all_edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(
        source: &std::path::Path,
        target: &std::path::Path,
        item_names: &[&str],
        item_kinds: Option<Vec<&str>>,
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "rust_ra_move_item_to_module".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            item_names: Some(item_names.iter().map(|name| name.to_string()).collect()),
            item_kinds: item_kinds.map(|kinds| kinds.into_iter().map(|k| k.to_string()).collect()),
            project_dir: None,
            ..Default::default()
        }
    }

    fn rust_analyzer_available() -> bool {
        Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn apply_plan(plan_text: &str) -> RefactorApplyResponse {
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(plan_text).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                cwd: None,
                force_path: None,
            },
            &[],
        )
        .unwrap();
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn rejects_impl_method_kind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.rs");
        let target = dir.path().join("target.rs");
        std::fs::write(&source, "fn moved() {}\n").unwrap();

        let err = plan_ra_move(&params(
            &source,
            &target,
            &["moved"],
            Some(vec!["impl_method"]),
        ))
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("impl_method_unsupported_in_ra_move"));
        assert!(msg.contains("extract_rust_impl_methods"));
    }

    #[test]
    fn lsp_unavailable_is_reported_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.rs");
        let target = dir.path().join("target.rs");
        std::fs::write(&source, "fn moved() {}\n").unwrap();
        std::fs::write(&target, "").unwrap();

        // Force an unavailable rust-analyzer by setting a bad binary path.
        unsafe {
            std::env::set_var(
                "BLACKBOX_RUST_ANALYZER_BIN",
                "__definitely_missing_binary__",
            );
        }
        let err = plan_ra_move(&params(&source, &target, &["moved"], None)).unwrap_err();
        unsafe {
            std::env::remove_var("BLACKBOX_RUST_ANALYZER_BIN");
        }
        let msg = err.to_string();
        assert!(msg.contains("error.lsp_unavailable"));
    }

    // TODO(RX-R1 LSP-integration): requires rust-analyzer with "Move to module"
    // code action surfaced and a buildable cargo project context; flaky outside CI.
    #[test]
    #[ignore]
    fn move_free_fn_updates_decl_and_call_sites() {
        if !rust_analyzer_available() {
            eprintln!(
                "skipping rust_ra_move_item_to_module free-fn test: rust-analyzer unavailable"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"ra-move-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let source = dir.path().join("src/main.rs");
        let target = dir.path().join("src/moved.rs");
        std::fs::write(
            &source,
            "pub fn moved() -> usize { 1 }\n\npub fn caller() -> usize { moved() }\n",
        )
        .unwrap();
        std::fs::write(&target, "").unwrap();

        let mut p = params(&source, &target, &["moved"], Some(vec!["function_item"]));
        p.project_dir = Some(dir.path().to_string_lossy().into_owned());
        let plan_text = plan_ra_move(&p).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan_value["semantic_status"], "lsp_verified");
        let response = apply_plan(&plan_text);
        assert_eq!(response.status, "ok");

        let source_after = std::fs::read_to_string(&source).unwrap();
        let target_after = std::fs::read_to_string(&target).unwrap();
        assert!(!source_after.contains("pub fn moved"));
        assert!(target_after.contains("pub fn moved"));
    }

    // TODO(RX-R1 LSP-integration): same rationale as move_free_fn_updates_*.
    #[test]
    #[ignore]
    fn move_type() {
        if !rust_analyzer_available() {
            eprintln!("skipping rust_ra_move_item_to_module type test: rust-analyzer unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"ra-move-type\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let source = dir.path().join("src/main.rs");
        let target = dir.path().join("src/types.rs");
        std::fs::write(
            &source,
            "pub struct User {\n    id: usize,\n}\n\npub fn builder() -> User {\n    User { id: 0 }\n}\n",
        )
        .unwrap();
        std::fs::write(&target, "").unwrap();

        let mut p = params(&source, &target, &["User"], Some(vec!["struct_item"]));
        p.project_dir = Some(dir.path().to_string_lossy().into_owned());
        let plan_text = plan_ra_move(&p).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan.semantic_status, SemanticStatus::LspVerified);
        let _ = apply_plan(&plan_text);

        let source_after = std::fs::read_to_string(&source).unwrap();
        let target_after = std::fs::read_to_string(&target).unwrap();
        assert!(!source_after.contains("pub struct User"));
        assert!(target_after.contains("pub struct User"));
    }
}
