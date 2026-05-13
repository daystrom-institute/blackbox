use super::*;

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn project_refs_returns_current_chunk_entity_refs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn alpha() -> i32 { 1 }\n\npub fn beta() -> i32 { alpha() + 1 }\n",
        )
        .unwrap();

        let text = project_refs(&RefactorProjectRefsParams {
            file: "src/lib.rs".into(),
            project_dir: Some(dir.path().to_string_lossy().to_string()),
            query: Some("alpha".into()),
            limit: Some(10),
            include_excerpt: Some(true),
        })
        .unwrap();
        let parsed: RefactorProjectRefs = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.relative_path, "src/lib.rs");
        assert_eq!(parsed.rel_path_hash, short_hash(b"src/lib.rs"));
        assert!(!parsed.chunks.is_empty());
        let chunk = &parsed.chunks[0];
        assert!(chunk.entity_ref.starts_with(&format!(
            "project_file:{}:{}:",
            parsed.project_id, parsed.rel_path_hash
        )));
        assert_eq!(chunk.chunk_hash.len(), 64);
        assert!(
            chunk
                .excerpt
                .as_deref()
                .unwrap_or_default()
                .contains("alpha")
        );
    }

    /// CN-D4 contract: project_refs records carry symbol_kind,
    /// parent_kind, line_start, and line_end whenever the chunk
    /// supplies them. The fields are optional in JSON
    /// (`skip_serializing_if`) so pre-CN-D4 callers continue to parse
    /// the new shape — but new callers can drive synthesis decisions
    /// without a follow-up bbox_refactor_status call.
    #[test]
    fn project_refs_records_carry_symbol_and_parent_kinds_with_line_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub struct S;\n\nimpl S {\n    pub fn run(&self) -> i32 { 1 }\n}\n",
        )
        .unwrap();

        let text = project_refs(&RefactorProjectRefsParams {
            file: "src/lib.rs".into(),
            project_dir: Some(dir.path().to_string_lossy().to_string()),
            query: None,
            limit: Some(20),
            include_excerpt: Some(false),
        })
        .unwrap();
        let parsed: RefactorProjectRefs = serde_json::from_str(&text).unwrap();
        assert!(parsed.status == "ok");

        // The Rust source above produces an `impl_item` chunk and a
        // `function_item` chunk inside it. The function_item record
        // must carry parent_kind = impl_item.
        let method_ref = parsed
            .chunks
            .iter()
            .find(|c| c.symbol_kind.as_deref() == Some("function_item"))
            .expect("function_item chunk present for impl method");
        assert_eq!(method_ref.parent_kind.as_deref(), Some("impl_item"));
        // Qualified name is `<impl header>::run`; bare name asserted via
        // the symbol field containing "run".
        assert!(
            method_ref
                .symbol
                .as_deref()
                .map(|s| s.contains("run"))
                .unwrap_or(false),
            "expected method symbol to contain 'run', got {:?}",
            method_ref.symbol
        );
        assert!(method_ref.line_start.is_some());
        assert!(method_ref.line_end.is_some());

        // Top-level struct: symbol_kind=struct_item, parent_kind=None.
        let struct_ref = parsed
            .chunks
            .iter()
            .find(|c| c.symbol_kind.as_deref() == Some("struct_item"))
            .expect("struct_item chunk present");
        assert_eq!(struct_ref.parent_kind, None);
        assert_eq!(struct_ref.line_start, Some(1));
    }

    #[test]
    fn status_lists_top_level_rust_items_with_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            "#[derive(Debug)]\npub struct Thing;\n\nfn helper() {}\n",
        )
        .unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.language, "rust");
        assert_eq!(parsed.parse.error_nodes, 0);
        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.kind == "struct_item" && item.name.as_deref() == Some("Thing"))
        );
        assert!(parsed.items.iter().any(|item| {
            item.attributes
                .iter()
                .any(|attr| attr == "#[derive(Debug)]")
        }));
    }

    #[test]
    fn status_lists_rust_impl_methods() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            "struct Server;\n\nimpl Server {\n    #[tool(description = \"x\")]\n    fn find(&self) {}\n}\n",
        )
        .unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        let method = parsed
            .items
            .iter()
            .find(|item| item.kind == "impl_method" && item.name.as_deref() == Some("find"))
            .expect("impl method should be listed");
        assert!(
            method
                .attributes
                .iter()
                .any(|attr| attr == "#[tool(description = \"x\")]")
        );
    }

    #[test]
    fn multiline_rust_attribute_moves_with_item() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "#[derive(\n    Debug,\n    Clone,\n)]\npub struct MoveMe;\n\nfn keep() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_items".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["MoveMe".into()]),
            item_kinds: Some(vec!["struct_item".into()]),
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("#[derive("));
        assert!(target_text.contains("pub struct MoveMe"));
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("#[derive("));
        assert!(source_text.contains("fn keep()"));
    }

    #[test]
    fn extract_impl_methods_wraps_target_router_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\n#[tool_router(router = old_tools)]\nimpl BlackboxServer {\n    #[tool(description = \"move\")]\n    fn move_me(&self) -> usize {\n        1\n    }\n\n    fn keep(&self) -> usize {\n        2\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("fn move_me"));
        assert!(source_text.contains("fn keep"));

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("use super::*;"));
        assert!(target_text.contains("#[tool_router(router = moved_tools)]"));
        assert!(target_text.contains("#[tool(description = \"move\")]"));
        assert!(target_text.contains("impl BlackboxServer"));
        assert!(target_text.contains("fn move_me"));
    }

    #[test]
    fn extract_impl_methods_preserves_async_modifier() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    #[tool(description = \"move\")]\n    async fn move_me(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        apply(
            &RefactorApplyParams {
                plan: plan_value,
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

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("async fn move_me"));
    }

    #[test]
    fn extract_impl_methods_rebases_super_paths_for_child_module_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("engine.rs");
        let target = dir.path().join("engine/fanout.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &source,
            "struct WorkflowRunner;\n\nimpl WorkflowRunner {\n    fn run_activity_node(&self) {\n        self.run_dynamic_fanout_node();\n    }\n\n    fn run_dynamic_fanout_node(&self) -> super::schema::NodeSpec {\n        super::compile(\"x\");\n        super::schema::NodeSpec::default()\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["run_dynamic_fanout_node".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl WorkflowRunner".into()),
            target_prelude: Some("use super::*;".into()),
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("pub(super) fn run_dynamic_fanout_node"));
        assert!(target_text.contains("-> super::super::schema::NodeSpec"));
        assert!(target_text.contains("super::super::compile(\"x\")"));
        assert!(!target_text.contains("super::super::super::schema"));
    }

    #[test]
    fn extract_impl_methods_can_generate_router_export_helper() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: Some("router".into()),
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("pub(super) fn router() -> ToolRouter<BlackboxServer>"));
        assert!(target_text.contains("BlackboxServer::moved_tools()"));
        assert!(target_text.contains("#[tool_router(router = moved_tools)]"));
    }

    #[test]
    fn extract_impl_methods_appends_to_existing_target_impl() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "use super::*;\n\n#[tool_router(router = moved_tools)]\nimpl BlackboxServer {\n    fn already_here(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert_eq!(target_text.matches("impl BlackboxServer").count(), 1);
        assert!(target_text.contains("fn already_here"));
        assert!(target_text.contains("fn move_me"));
    }

    #[test]
    fn extract_impl_methods_does_not_merge_different_router_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "#[tool_router(router = search_tools_extra)]\nimpl BlackboxServer {\n    fn already_here(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert_eq!(target_text.matches("impl BlackboxServer").count(), 2);
        assert!(target_text.contains("#[tool_router(router = search_tools)]"));
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_at_top_of_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(&target, "pub fn helper() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.starts_with("use super::*;\n\npub fn helper()"));
        assert!(target_text.contains("impl BlackboxServer"));
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_after_inner_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "#![allow(dead_code)]\n//! module docs\n\n// use super::*;\npub fn helper() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.starts_with("#![allow(dead_code)]\n//! module docs\n\nuse super::*;"));
        assert_eq!(target_text.matches("use super::*;").count(), 2);
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_after_inner_block_doc() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(&target, "/*!\nmodule docs\n*/\n\npub fn helper() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(
            fs::read_to_string(&target)
                .unwrap()
                .starts_with("/*!\nmodule docs\n*/\n\nuse super::*;")
        );
    }

    #[test]
    fn extract_impl_methods_handles_generic_impl_header() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "struct Boxed<T>(T);\n\nimpl<T> Boxed<T>\nwhere\n    T: Clone,\n{\n    fn clone_inner(&self) -> T {\n        self.0.clone()\n    }\n}\n",
        )
        .unwrap();

        let header = "impl<T> Boxed<T> where T: Clone,";
        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["clone_inner".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some(header.into()),
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&target).unwrap().contains(header));
    }

    #[test]
    fn extract_impl_method_requires_impl_filter_when_method_name_is_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn same(&self) {} }\nimpl B { fn same(&self) {} }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["same".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("matched multiple impl blocks"));
    }

    #[test]
    fn extract_impl_method_rejects_misleading_function_item_kind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(&source, "struct A;\nimpl A { fn method(&self) {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["method".into()]),
            item_kinds: Some(vec!["function_item".into()]),
            impl_name: Some("impl A".into()),
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("only supports item_kinds impl_method")
        );
    }

    #[test]
    fn delete_rust_items_removes_top_level_items_only() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod remove_me;\nmod keep_mod;\n\n#[derive(Debug)]\nstruct DeleteMe;\n\nfn keep() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["remove_me".into(), "DeleteMe".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("remove_me"));
        assert!(!source_text.contains("DeleteMe"));
        assert!(!source_text.contains("#[derive(Debug)]"));
        assert!(source_text.contains("mod keep_mod;"));
        assert!(source_text.contains("fn keep()"));
    }

    #[test]
    fn delete_rust_items_removes_impl_method_with_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nimpl A {\n    #[allow(dead_code)]\n    fn delete_me(&self) {}\n\n    fn keep(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["delete_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl A".into()),
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("delete_me"));
        assert!(!source_text.contains("#[allow(dead_code)]"));
        assert!(source_text.contains("fn keep"));
    }

    #[test]
    fn delete_rust_items_reports_leftovers_within_impl_scope() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn delete_me(&self) {} fn keep_a(&self) {} }\nimpl B { fn keep_b(&self) {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["delete_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl A".into()),
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let leftovers = plan_value["leftovers"].as_array().unwrap();
        assert!(
            leftovers
                .iter()
                .any(|leftover| leftover.as_str().unwrap().contains("keep_a"))
        );
        assert!(
            !leftovers
                .iter()
                .any(|leftover| leftover.as_str().unwrap().contains("keep_b"))
        );
    }

    #[test]
    fn delete_rust_items_requires_impl_filter_for_ambiguous_method_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn same(&self) {} }\nimpl B { fn same(&self) {} }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["same".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("matched multiple impl blocks"));
    }

    #[test]
    fn delete_rust_items_requires_explicit_item_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "mod remove_me;\nmod keep_mod;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: Some(vec!["mod_item".into()]),
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("requires non-empty item_names"));
    }

    #[test]
    fn delete_rust_items_rejects_mixed_top_level_and_impl_method_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "struct A;\nimpl A { fn method(&self) {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["method".into()]),
            item_kinds: Some(vec!["struct_item".into(), "impl_method".into()]),
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("cannot mix impl_method"));
    }

    #[test]
    fn refactor_run_applies_sequential_plan_steps() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "add then delete module declaration".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: Some(vec!["newmod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "ok");
        assert_eq!(run_response.steps.len(), 2);
        assert!(run_response.steps.iter().all(|step| step.status == "ok"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_expands_split_rust_impl_methods_to_submodule() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("engine.rs");
        fs::write(
            &source,
            "struct WorkflowRunner;\n\nimpl WorkflowRunner {\n    fn run_fanout(&self) {}\n}\n",
        )
        .unwrap();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "targeted_tests".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("run_fanout".to_string())]),
        );

        let response = run(
            &RefactorRunParams {
                title: "split fanout methods".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Plan {
                    optional: false,
                    params: RefactorPlanParams {
                        kind: "split_rust_impl_methods_to_submodule".into(),
                        source: "engine.rs".into(),
                        target: Some("engine/fanout.rs".into()),
                        item_names: Some(vec!["run_fanout".into()]),
                        impl_name: Some("impl WorkflowRunner".into()),
                        toml_entries: Some(entries),
                        ..Default::default()
                    },
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "planned");
        let kinds = run_response
            .steps
            .iter()
            .filter_map(|step| step.kind.as_deref())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"add_rust_mod_decl"));
        assert!(kinds.contains(&"extract_rust_impl_methods"));
        assert!(kinds.contains(&"rust_compile_fix_round"));
        assert!(
            run_response
                .steps
                .iter()
                .any(|step| { step.title.as_deref() == Some("cargo check --message-format=json") })
        );
        assert!(
            run_response
                .steps
                .iter()
                .any(|step| { step.title.as_deref() == Some("cargo test run_fanout") })
        );
    }

    #[test]
    fn refactor_run_expands_rust_minimize_imports_with_organize_imports() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(src_dir.join("parent")).unwrap();
        fs::write(&src_dir.join("parent.rs"), "pub(crate) struct Thing;\n").unwrap();
        fs::write(
            src_dir.join("parent").join("child.rs"),
            "use super::*;\n\nfn run(_thing: Thing) {}\n",
        )
        .unwrap();

        let response = run(
            &RefactorRunParams {
                title: "minimize imports".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Plan {
                    optional: false,
                    params: RefactorPlanParams {
                        kind: "rust_minimize_imports".into(),
                        source: "src/parent/child.rs".into(),
                        ..Default::default()
                    },
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();

        assert_eq!(run_response.status, "planned");
        let kinds = run_response
            .steps
            .iter()
            .filter_map(|step| step.kind.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(kinds[0], "rust_minimize_imports");
        assert!(kinds.contains(&"rust_organize_imports"));
    }

    #[test]
    fn rewrite_rust_bin_crate_paths_rewrites_simple_and_grouped_imports() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let main = src_dir.join("main.rs");
        fs::write(
            &main,
            "use crate::{alpha, beta};\nfn run() { crate::alpha::go(); crate::beta::go(); }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_bin_crate_paths".into(),
            source: path_string(&main),
            item_names: Some(vec!["alpha".into(), "beta".into()]),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(plan.kind, "rewrite_rust_bin_crate_paths");
        let replacements = plan
            .edits
            .first()
            .unwrap()
            .edits
            .iter()
            .map(|edit| edit.replacement.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            replacements,
            vec!["demo_app", "demo_app::alpha", "demo_app::beta"]
        );
    }

    #[test]
    fn refactor_run_expands_migrate_rust_mods_to_lib() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"demo\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("main.rs"),
            "mod alpha;\nfn run() { crate::alpha::go(); }\n",
        )
        .unwrap();
        fs::write(src_dir.join("lib.rs"), "").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "migrate alpha".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Plan {
                    optional: false,
                    params: RefactorPlanParams {
                        kind: "migrate_rust_mods_to_lib".into(),
                        source: "src/main.rs".into(),
                        item_names: Some(vec!["alpha".into()]),
                        ..Default::default()
                    },
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();

        assert_eq!(run_response.status, "planned");
        let kinds = run_response
            .steps
            .iter()
            .filter_map(|step| step.kind.as_deref())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"copy_rust_mod_decls"));
        assert!(kinds.contains(&"delete_rust_items"));
        assert!(kinds.contains(&"rewrite_rust_bin_crate_paths"));
        assert!(kinds.contains(&"rust_compile_fix_round"));
        assert!(
            run_response
                .steps
                .iter()
                .any(|step| { step.title.as_deref() == Some("cargo check --bins") })
        );
    }

    #[test]
    fn refactor_run_rolls_back_when_later_plan_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback failed compound run".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: Some(vec!["missing_mod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_optional_plan_skips_on_failure_keeps_prior_writes() {
        // Gap 2: a batch where one step's plan returns "no boilerplate"
        // should not undo earlier successful writes. Marking the failing
        // step `optional: true` turns the failure into a logged skip
        // and continues to the next step.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        // Use `delete_rust_items` against a non-existent item as the
        // failing plan kind — same shape as lombokify's "no boilerplate"
        // bail in the wild but works in the rust-only test harness.
        let response = run(
            &RefactorRunParams {
                title: "optional skip preserves prior writes".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    // Step 0: succeeds, writes to lib.rs.
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            module_name: Some("preserved_mod".into()),
                            ..Default::default()
                        },
                    },
                    // Step 1: optional, fails (no item to delete) — should be skipped, not abort.
                    RefactorRunStep::Plan {
                        optional: true,
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            item_names: Some(vec!["nonexistent_item".into()]),
                            item_kinds: Some(vec!["fn_item".into()]),
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "ok", "batch must succeed: {response}");
        assert!(
            !run_response.rolled_back,
            "prior writes must be preserved, not rolled back"
        );
        assert_eq!(run_response.steps.len(), 2);
        assert_eq!(run_response.steps[0].status, "ok");
        assert_eq!(
            run_response.steps[1].status, "skipped",
            "optional failing step must be marked skipped"
        );
        assert!(
            run_response.steps[1].error.is_some(),
            "skipped step must carry the original error message"
        );
        // Step 0's write survived.
        let final_text = fs::read_to_string(&source).unwrap();
        assert!(
            final_text.contains("mod preserved_mod"),
            "step 0's write must survive optional skip: {final_text}"
        );
    }

    #[test]
    fn refactor_run_non_optional_plan_failure_still_rolls_back() {
        // Default `optional: false` preserves the strict batch semantic.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "default strict batch".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            module_name: Some("transient".into()),
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Plan {
                        optional: false, // explicit; same as default
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            item_names: Some(vec!["nonexistent_item".into()]),
                            item_kinds: Some(vec!["fn_item".into()]),
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_rolls_back_when_later_path_is_out_of_scope() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let outside = outside_dir.path().join("outside.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();
        fs::write(&outside, "mod outside_mod;\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback out of scope step".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: path_string(&outside),
                            target: None,
                            item_names: Some(vec!["outside_mod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(
            run_response
                .error
                .unwrap()
                .contains("outside registered projects")
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "mod outside_mod;\n");
    }

    #[test]
    fn refactor_run_rolls_back_file_move_when_later_plan_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("packets.rs");
        let target = dir.path().join("packets").join("mod.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback moved file".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "move_file".into(),
                            source: "packets.rs".into(),
                            target: Some("packets/mod.rs".into()),
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: None,
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "packets/mod.rs".into(),
                            target: None,
                            item_names: Some(vec!["missing".into()]),
                            item_kinds: Some(vec!["function_item".into()]),
                            impl_name: None,
                            module_name: None,
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
        assert!(!target.exists());
    }

    #[test]
    fn refactor_run_rolls_back_when_required_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback failed command".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Command {
                        command: "false".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_reports_command_with_embedded_args_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback malformed command".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
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
                            project_dir: None,
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Command {
                        command: "cargo fmt".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(
            run_response.steps[1]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("put arguments in args")
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_rolls_back_declared_command_touches() {
        let dir = tempfile::tempdir().unwrap();
        let generated = dir.path().join("generated.txt");

        let response = run(
            &RefactorRunParams {
                title: "rollback command side effects".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "sh".into(),
                    args: vec!["-c".into(), "printf created > generated.txt; false".into()],
                    cwd: None,
                    touches: vec!["generated.txt".into()],
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(!generated.exists());
    }

    #[test]
    fn command_output_truncation_preserves_failure_tail() {
        let output = (0..200)
            .map(|idx| format!("line {idx}"))
            .chain(std::iter::once("failures: important_test".to_string()))
            .collect::<Vec<_>>()
            .join("\n");

        let truncated = truncate_for_report(&output, 120);

        assert!(truncated.contains("line 0"));
        assert!(truncated.contains("[truncated middle]"));
        assert!(truncated.contains("failures: important_test"));
        assert!(truncated.chars().count() <= 120);
    }

    #[test]
    fn rewrite_rust_item_visibility_updates_top_level_items() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "struct Hidden;\nfn helper() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_item_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["Hidden".into(), "helper".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub(super)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let text = fs::read_to_string(&source).unwrap();
        assert!(text.contains("pub(super) struct Hidden;"));
        assert!(text.contains("pub(super) fn helper() {}"));
    }

    #[test]
    fn rewrite_rust_item_visibility_updates_impl_methods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct Thing;\nimpl Thing { fn hidden(&self) {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_item_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["hidden".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Thing".into()),
            module_name: None,
            visibility: Some("pub(super)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(
            fs::read_to_string(&source)
                .unwrap()
                .contains("impl Thing { pub(super) fn hidden(&self) {} }")
        );
    }

    #[test]
    fn rewrite_rust_item_visibility_updates_async_impl_methods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct Thing;\nimpl Thing { async fn hidden(&self) {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_item_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["hidden".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Thing".into()),
            module_name: None,
            visibility: Some("pub(super)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(
            fs::read_to_string(&source)
                .unwrap()
                .contains("impl Thing { pub(super) async fn hidden(&self) {} }")
        );
    }

    #[test]
    fn rewrite_rust_item_visibility_can_update_all_methods_in_impl() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct Thing;\nimpl Thing { fn hidden(&self) {} fn also_hidden(&self) {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_item_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl Thing".into()),
            module_name: None,
            visibility: Some("pub(crate)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let text = fs::read_to_string(&source).unwrap();
        assert!(text.contains("pub(crate) fn hidden(&self) {}"));
        assert!(text.contains("pub(crate) fn also_hidden(&self) {}"));
    }

    #[test]
    fn rewrite_rust_field_visibility_updates_named_struct_fields() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "#[derive(Default)]\nstruct SharedState {\n    artifacts: usize,\n    #[allow(dead_code)]\n    task_store: String,\n    pub already_public: bool,\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_field_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["SharedState".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub(crate)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let text = fs::read_to_string(&source).unwrap();
        assert!(text.contains("pub(crate) artifacts: usize"));
        assert!(text.contains("#[allow(dead_code)]\n    pub(crate) task_store: String"));
        assert!(text.contains("pub(crate) already_public: bool"));
    }

    #[test]
    fn add_rust_router_to_sum_appends_router_call() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server {\n    fn new() -> Self {\n        Self {\n            tool_router: Self::bbox_tools() + Self::bro_tools(),\n        }\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(source_text.contains(
            "tool_router: Self::bbox_tools() + Self::bro_tools() + Self::search_tools(),"
        ));
    }

    #[test]
    fn add_rust_router_to_sum_accepts_module_router_call() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server {\n    fn new() -> Self {\n        Self {\n            tool_router: Self::bbox_tools() + Self::bro_tools(),\n        }\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: Some("refactor_tools::router()".into()),
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(source_text.contains(
            "tool_router: Self::bbox_tools() + Self::bro_tools() + refactor_tools::router(),"
        ));
    }

    #[test]
    fn add_rust_mod_decl_appends_after_existing_mods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(&source, "mod alpha;\nmod beta;\n\nuse std::fmt;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("gamma".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\nmod beta;\nmod gamma;\n\nuse std::fmt;\n"
        );
    }

    #[test]
    fn add_rust_mod_decl_ignores_inline_modules_for_insert_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "mod alpha;\n\nfn main() {}\n\nmod tests { fn helper() {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("server".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\nmod server;\n\nfn main() {}\n\nmod tests { fn helper() {} }\n"
        );
    }

    #[test]
    fn add_rust_mod_decl_rejects_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(&source, "mod alpha;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("alpha".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn add_rust_mod_decl_supports_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub mod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("beta".into()),
            visibility: Some("pub(crate)".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "pub mod alpha;\npub(crate) mod beta;\n"
        );
    }

    #[test]
    fn copy_rust_mod_decls_copies_selected_declarations_with_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod alpha;\nmod beta;\nmod inline { fn no_copy() {} }\n\nfn main() {}\n",
        )
        .unwrap();
        fs::write(&target, "pub mod existing;\n\npub use existing::Thing;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["alpha".into(), "beta".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "pub mod existing;\npub mod alpha;\npub mod beta;\n\npub use existing::Thing;\n"
        );
    }

    #[test]
    fn copy_rust_mod_decls_rejects_inline_module() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(&source, "mod inline { fn no_copy() {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["inline".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("is inline"));
    }

    #[test]
    fn rust_minimize_imports_replaces_super_wildcard_with_used_names() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(src_dir.join("parent")).unwrap();
        let parent = src_dir.join("parent.rs");
        let child = src_dir.join("parent").join("child.rs");
        fs::write(
            &parent,
            "pub(crate) struct Thing;\npub(crate) fn helper() {}\npub(crate) fn unused() {}\n",
        )
        .unwrap();
        fs::write(
            &child,
            "use super::*;\n\nfn run() {\n    let _thing = Thing;\n    helper();\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rust_minimize_imports".into(),
            source: path_string(&child),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(plan.kind, "rust_minimize_imports");
        assert_eq!(plan.edits[0].edits.len(), 1);
        assert_eq!(
            plan.edits[0].edits[0].replacement,
            "use super::{Thing, helper};"
        );
        assert!(plan.leftovers.is_empty());
    }

    #[test]
    fn rust_minimize_imports_resolves_sibling_module_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(src_dir.join("parent")).unwrap();
        fs::write(&src_dir.join("parent.rs"), "mod helpers;\nmod child;\n").unwrap();
        fs::write(
            src_dir.join("parent").join("helpers.rs"),
            "pub(super) enum Mode { Fast }\npub(super) fn unused() {}\n",
        )
        .unwrap();
        let child = src_dir.join("parent").join("child.rs");
        fs::write(
            &child,
            "use super::helpers::*;\n\nfn run(mode: Mode) {\n    match mode { Mode::Fast => {} }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rust_minimize_imports".into(),
            source: path_string(&child),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(
            plan.edits[0].edits[0].replacement,
            "use super::helpers::{Mode};"
        );
    }

    #[test]
    fn rust_minimize_imports_preserves_unproven_unused_wildcard_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(src_dir.join("parent")).unwrap();
        fs::write(
            &src_dir.join("parent.rs"),
            "pub(crate) trait Extension {}\n",
        )
        .unwrap();
        let child = src_dir.join("parent").join("child.rs");
        fs::write(&child, "use super::*;\n\nfn run() {}\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "rust_minimize_imports".into(),
            source: path_string(&child),
            project_dir: Some(path_string(dir.path())),
            ..Default::default()
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("no wildcard imports could be minimized")
        );
        assert!(
            err.to_string()
                .contains("no directly referenced names found")
        );
    }

    #[test]
    fn extract_rust_function_region_inserts_helper_and_replaces_selection() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let selected = "let value = input + 1;\n    println!(\"{value}\");";
        fs::write(
            &source,
            "fn run(input: i32) {\n    let value = input + 1;\n    println!(\"{value}\");\n}\n",
        )
        .unwrap();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "parameters".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("input: i32".to_string())]),
        );
        entries.insert(
            "arguments".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("input".to_string())]),
        );

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_function_region".into(),
            source: path_string(&source),
            old_text: Some(selected.into()),
            item_names: Some(vec!["print_value".into()]),
            toml_entries: Some(entries),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(plan.kind, "extract_rust_function_region");
        assert_eq!(plan.edits[0].edits[0].replacement, "print_value(input);");
        assert!(
            plan.edits[0].edits[1]
                .replacement
                .contains("fn print_value(input: i32)")
        );
    }

    #[test]
    fn extract_rust_function_region_rejects_early_return() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "fn run(input: i32) -> i32 {\n    if input < 0 { return 0; }\n    input\n}\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "extract_rust_function_region".into(),
            source: path_string(&source),
            old_text: Some("if input < 0 { return 0; }".into()),
            item_names: Some(vec!["guard".into()]),
            ..Default::default()
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("rejects regions containing `return`")
        );
    }

    #[test]
    fn extract_rust_function_region_uses_self_call_inside_impl() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Runner;\nimpl Runner {\n    fn run(&self, input: i32) {\n        println!(\"{}\", input + 1);\n    }\n}\n",
        )
        .unwrap();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "parameters".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("input: i32".to_string())]),
        );
        entries.insert(
            "arguments".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("input".to_string())]),
        );

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_function_region".into(),
            source: path_string(&source),
            old_text: Some("println!(\"{}\", input + 1);".into()),
            item_names: Some(vec!["print_value".into()]),
            toml_entries: Some(entries),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(
            plan.edits[0].edits[0].replacement,
            "Self::print_value(input);"
        );
    }

    #[test]
    fn migrate_rust_string_field_to_enum_generates_serde_enum() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("params.rs");
        fs::write(
            &source,
            "use serde::{Deserialize, Serialize};\n\n#[derive(Debug, Deserialize)]\npub struct Params {\n    pub kind: String,\n}\n",
        )
        .unwrap();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "field_name".into(),
            serde_json::Value::String("kind".into()),
        );
        entries.insert(
            "enum_name".into(),
            serde_json::Value::String("PlanKind".into()),
        );
        entries.insert(
            "variants".into(),
            serde_json::json!([
                {"name": "ExtractRustItems", "rename": "extract_rust_items"},
                {"name": "DeleteRustItems", "rename": "delete_rust_items", "aliases": ["delete_items"]}
            ]),
        );

        let plan_text = plan(&RefactorPlanParams {
            kind: "migrate_rust_string_field_to_enum".into(),
            source: path_string(&source),
            toml_entries: Some(entries),
            ..Default::default()
        })
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        assert_eq!(plan.kind, "migrate_rust_string_field_to_enum");
        assert!(
            plan.edits[0].edits[0]
                .replacement
                .contains("pub enum PlanKind")
        );
        assert!(
            plan.edits[0].edits[0]
                .replacement
                .contains("alias = \"delete_items\"")
        );
        assert_eq!(plan.edits[0].edits[1].replacement, "pub kind: PlanKind,");
    }

    #[test]
    fn copy_rust_mod_decls_creates_missing_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(&source, "mod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["alpha".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&target).unwrap(), "pub mod alpha;\n");
    }

    #[test]
    fn rewrite_rust_mod_visibility_updates_existing_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod alpha;\npub(crate) mod beta;\npub mod gamma;\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_mod_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["beta".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\npub mod beta;\npub mod gamma;\n"
        );
    }

    #[test]
    fn rewrite_rust_mod_visibility_preserves_attached_attribute() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "#[path = \"alpha_impl.rs\"]\nmod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_mod_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["alpha".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "#[path = \"alpha_impl.rs\"]\npub mod alpha;\n"
        );
    }

    #[test]
    fn move_file_moves_source_to_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("packets.rs");
        let target = dir.path().join("packets").join("mod.rs");
        fs::write(&source, "pub fn packet() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "move_file".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan_value["kind"], "move_file");
        assert_eq!(plan_value["file_moves"].as_array().unwrap().len(), 1);
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "pub fn packet() {}\n");
    }

    #[test]
    fn move_file_rejects_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.rs");
        let target = dir.path().join("b.rs");
        fs::write(&source, "pub fn a() {}\n").unwrap();
        fs::write(&target, "pub fn b() {}\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "move_file".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("target already exists"));
    }

    #[test]
    fn replace_text_replaces_exact_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub fn before() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "replace_text".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some("before".into()),
            new_text: Some("after".into()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&source).unwrap(), "pub fn after() {}\n");
    }

    #[test]
    fn write_file_creates_missing_supported_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src").join("lib.rs");

        let plan_text = plan(&RefactorPlanParams {
            kind: "write_file".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: Some("pub mod packets;\n".into()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&source).unwrap(), "pub mod packets;\n");
    }

    // Gap 1: validate_rewritten_files surfaces line+excerpt for the first few
    // ERROR / MISSING nodes so the operator can debug `validation_failed`
    // without re-running the plan.
    #[test]
    fn validate_rewritten_files_emits_error_excerpts_for_broken_java() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Broken.java");
        let source = "package x;\n\
                      public class Broken {\n\
                          public void m() {\n\
                              foo bar baz quux ;;;\n\
                          }\n\
                      }\n";
        let results = validate_rewritten_files(&[(path.clone(), source.as_bytes().to_vec())])
            .expect("validation pass should succeed even when source has parse errors");
        assert_eq!(results.len(), 1, "one input file → one validation result");
        let result = &results[0];
        assert!(
            result.has_error,
            "deliberately broken source must flag has_error"
        );
        assert!(
            !result.error_excerpts.is_empty(),
            "broken source must surface at least one excerpt"
        );
        let first = &result.error_excerpts[0];
        assert!(
            first.kind == "error" || first.kind == "missing",
            "excerpt kind = {}",
            first.kind
        );
        assert!(first.line >= 1, "1-based line numbering");
        assert!(first.column >= 1, "1-based column numbering");
        assert!(first.byte_end >= first.byte_start);
        assert!(
            first.snippet.contains(" | "),
            "snippet should include line-number gutter: {}",
            first.snippet
        );
    }

    // Gap 1: clean files keep error_excerpts empty so the field skip-if-empty
    // serde attribute keeps responses tidy.
    #[test]
    fn validate_rewritten_files_excerpts_empty_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Clean.java");
        let source = "package x;\npublic class Clean {}\n";
        let results = validate_rewritten_files(&[(path, source.as_bytes().to_vec())]).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].has_error);
        assert!(results[0].error_excerpts.is_empty());
    }

    // Gap 1: tree-sitter can surface many ERROR nodes for one syntactic
    // injury; we cap excerpts so the response payload stays bounded.
    #[test]
    fn validate_rewritten_files_caps_error_excerpts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Many.java");
        // A pile of stray tokens at top level — tree-sitter-java produces an
        // ERROR node per stray construct.
        let mut source = String::from("package x;\n");
        for _ in 0..20 {
            source.push_str("@@@\n");
        }
        source.push_str("public class Many {}\n");
        let results = validate_rewritten_files(&[(path, source.into_bytes())]).unwrap();
        let result = &results[0];
        assert!(result.has_error);
        assert!(
            result.error_excerpts.len() <= PARSE_ERROR_EXCERPT_LIMIT,
            "error_excerpts.len() = {} should be capped at {}",
            result.error_excerpts.len(),
            PARSE_ERROR_EXCERPT_LIMIT
        );
    }

    #[test]
    fn ensure_toml_table_adds_lib_table() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Cargo.toml");
        fs::write(
            &source,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"demo\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "ensure_toml_table".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: Some("lib".into()),
            toml_entries: Some(BTreeMap::from([
                ("name".into(), serde_json::json!("demo")),
                ("path".into(), serde_json::json!("src/lib.rs")),
            ])),
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let updated = fs::read_to_string(&source).unwrap();
        assert!(updated.contains("[lib]\nname = \"demo\"\npath = \"src/lib.rs\"\n"));
        updated.parse::<toml::Value>().unwrap();
    }

    // Cold-start rust-analyzer against a fresh tempdir is timing-fragile in
    // CI: cargo metadata + crate-graph build runs silently for several
    // seconds before rust-analyzer starts emitting serverStatus or
    // publishDiagnostics. The session manager has both wait paths
    // (`wait_for_rust_analyzer_ready` post-init, `wait_for_diagnostics`
    // post-`didOpen`) so production warm sessions work, but this unit test
    // sees the rename request hit before any analysis signal fires and
    // returns "No references found at position". Keep #[ignore] until we
    // either (a) drive a deterministic ready signal from rust-analyzer or
    // (b) accept a fixed cold-start sleep.
    #[test]
    #[ignore]
    fn rust_lsp_rename_renames_references() {
        if !Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("skipping rust_lsp_rename test: rust-analyzer unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rename_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let source = dir.path().join("src").join("lib.rs");
        fs::write(
            &source,
            "pub fn old_name() -> usize { 1 }\n\npub fn caller() -> usize { old_name() }\n",
        )
        .unwrap();

        let ctx = PlanContext {
            lsp: Some(crate::lsp::LspSessionManager::new()),
        };
        let plan_text = plan_with_ctx(
            &RefactorPlanParams {
                kind: "rust_lsp_rename".into(),
                source: path_string(&source),
                target: None,
                item_names: Some(vec!["old_name".into()]),
                item_kinds: None,
                impl_name: None,
                module_name: None,
                visibility: None,
                use_path: None,
                router_name: None,
                router_call: None,
                router_export_name: None,
                target_prelude: None,
                old_text: None,
                new_text: Some("new_name".into()),
                replace_all: None,
                toml_table: None,
                toml_entries: None,
                project_dir: Some(path_string(dir.path())),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan_value["semantic_status"], "lsp_verified");
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let updated = fs::read_to_string(&source).unwrap();
        assert!(updated.contains("pub fn new_name()"));
        assert!(updated.contains("new_name() }"));
        assert!(!updated.contains("old_name"));
    }

    #[test]
    fn add_rust_use_decl_inserts_after_existing_uses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "mod alpha;\n\nuse std::fmt;\n\nfn main() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_use_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: Some("crate::alpha::Thing".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\n\nuse std::fmt;\nuse crate::alpha::Thing;\n\nfn main() {}\n"
        );
    }

    #[test]
    fn add_rust_use_decl_supports_pub_use_and_rejects_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub use crate::alpha::Thing;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_use_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: Some("crate::alpha::Thing".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn add_rust_router_to_sum_rejects_duplicate_router() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server { fn new() -> Self { Self { tool_router: Self::bbox_tools() + Self::search_tools(), } } }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("already contains"));
    }

    #[test]
    fn status_lists_generic_tree_sitter_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.ts");
        fs::write(&path, "export function helper() { return 1; }\n").unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.language, "typescript");
        assert_eq!(parsed.parse.error_nodes, 0);
        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.kind.contains("export") || item.name.as_deref() == Some("helper"))
        );
    }

    #[test]
    fn extract_plan_moves_named_item_and_apply_writes_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(&source, "fn keep() {}\n\nfn move_me() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_items".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
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
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&source).unwrap().contains("keep"));
        assert!(!fs::read_to_string(&source).unwrap().contains("move_me"));
        assert!(fs::read_to_string(&target).unwrap().contains("move_me"));
    }

    #[test]
    fn extract_rust_items_inserts_target_prelude() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(&source, "fn keep() {}\n\nfn move_me() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_items".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            ..Default::default()
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
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
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.starts_with("use super::*;\n\nfn move_me()"));
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = vec![
            TextEdit {
                byte_start: 0,
                byte_end: 5,
                replacement: String::new(),
            },
            TextEdit {
                byte_start: 4,
                byte_end: 6,
                replacement: String::new(),
            },
        ];
        assert!(ensure_non_overlapping(&edits).is_err());
    }

    #[test]
    fn apply_text_edits_sorts_unsorted_non_overlapping_edits() {
        let edits = vec![
            TextEdit {
                byte_start: 6,
                byte_end: 11,
                replacement: "earth".into(),
            },
            TextEdit {
                byte_start: 0,
                byte_end: 5,
                replacement: "hello".into(),
            },
        ];
        assert_eq!(
            apply_text_edits("world there", &edits).unwrap(),
            "hello earth"
        );
    }

    #[test]
    fn selecting_without_filters_is_rejected() {
        let items = vec![SyntaxItem {
            plan_local_id: "x".into(),
            kind: "function_item".into(),
            name: Some("f".into()),
            byte_start: 0,
            byte_end: 6,
            leading_trivia_start: 0,
            trailing_trivia_end: 7,
            line_start: 1,
            line_end: 1,
            attributes: Vec::new(),
        }];
        assert!(select_items(&items, None, None).is_err());
    }

    #[test]
    fn apply_rejects_paths_outside_registered_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("lib.rs");
        fs::write(&source, "fn f() {}\n").unwrap();
        let plan = RefactorPlan {
            title: "bad".into(),
            kind: "extract_rust_items".into(),
            semantic_status: SemanticStatus::SyntaxOnly,
            dry_run: true,
            file_moves: Vec::new(),
            edits: vec![FileEdit {
                path: path_string(&source),
                original_sha256: sha256_hex(b"fn f() {}\n"),
                edits: Vec::new(),
                new_text: None,
            }],
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
        };
        let err = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
                cwd: None,
                force_path: None,
            },
            &[project_record(project.path())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside registered projects"));
    }

    #[test]
    fn apply_can_allow_unregistered_paths_for_practice_worktrees() {
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("lib.rs");
        fs::write(&source, "fn f() {}\n").unwrap();
        let plan = RefactorPlan {
            title: "practice".into(),
            kind: "extract_rust_items".into(),
            semantic_status: SemanticStatus::SyntaxOnly,
            dry_run: true,
            file_moves: Vec::new(),
            edits: vec![FileEdit {
                path: path_string(&source),
                original_sha256: sha256_hex(b"fn f() {}\n"),
                edits: vec![TextEdit {
                    byte_start: 3,
                    byte_end: 4,
                    replacement: "g".into(),
                }],
                new_text: None,
            }],
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
        };

        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
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

        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(source).unwrap(), "fn g() {}\n");
    }
}

#[cfg(test)]
mod rx_f1a_taxonomy_tests {
    use super::*;

    #[test]
    fn syntax_only_roundtrips() {
        let s = serde_json::to_string(&SemanticStatus::SyntaxOnly).unwrap();
        assert_eq!(s, r#""syntax_only""#);
        let back: SemanticStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SemanticStatus::SyntaxOnly);
    }

    #[test]
    fn indexed_hints_roundtrips() {
        let s = serde_json::to_string(&SemanticStatus::IndexedHints).unwrap();
        assert_eq!(s, r#""indexed_hints""#);
        let back: SemanticStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SemanticStatus::IndexedHints);
    }

    #[test]
    fn lsp_verified_roundtrips() {
        let s = serde_json::to_string(&SemanticStatus::LspVerified).unwrap();
        assert_eq!(s, r#""lsp_verified""#);
        let back: SemanticStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SemanticStatus::LspVerified);
    }

    #[test]
    fn unverified_alias_deserializes_to_indexed_hints() {
        let back: SemanticStatus = serde_json::from_str(r#""unverified""#).unwrap();
        assert_eq!(back, SemanticStatus::IndexedHints);
    }
}

#[cfg(test)]
mod rx_f1b_plan_slot_tests {
    use crate::refactor::plan_slot;
    use std::env;

    fn with_state_dir<F: FnOnce()>(dir: &std::path::Path, f: F) {
        let _lock = crate::util::test_env_lock();
        let old = env::var("BLACKBOX_STATE_DIR").ok();
        unsafe { env::set_var("BLACKBOX_STATE_DIR", dir) };
        f();
        match old {
            Some(v) => unsafe { env::set_var("BLACKBOX_STATE_DIR", v) },
            None => unsafe { env::remove_var("BLACKBOX_STATE_DIR") },
        }
    }

    #[test]
    fn output_path_resolves_under_slot() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let resolved = plan_slot::resolve_plan_write_path("my-plan.json").unwrap();
            let slot = plan_slot::ensure_plan_slot().unwrap();
            assert!(
                resolved.starts_with(&slot),
                "expected {} to be under slot {}",
                resolved.display(),
                slot.display()
            );
            assert!(resolved.ends_with("my-plan.json"));
        });
    }

    #[test]
    fn output_path_absolute_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = plan_slot::resolve_plan_write_path("/tmp/x.json").unwrap_err();
            assert!(
                err.to_string().contains("plan_path_outside_slot"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn output_path_escape_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = plan_slot::resolve_plan_write_path("../../etc/passwd").unwrap_err();
            assert!(
                err.to_string().contains("plan_path_outside_slot"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn plan_path_read_escape_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = plan_slot::resolve_plan_read_path("../../foo.json").unwrap_err();
            assert!(
                err.to_string().contains("plan_path_outside_slot"),
                "unexpected error: {err}"
            );
        });
    }
}

#[cfg(test)]
mod rx_f2b_obligation_tests {
    use super::*;
    use std::fs;

    fn project_record(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path).unwrap().display().to_string(),
            registered_at: "2026-05-09T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
        }
    }

    /// Write a shell script that exits 1 and prints N compiler-message JSON lines.
    fn make_failing_capture_script(
        dir: &std::path::Path,
        name: &str,
        errors: usize,
        warnings: usize,
    ) -> std::path::PathBuf {
        let data_file = dir.join(format!("{name}_data.txt"));
        let script = dir.join(format!("{name}.sh"));
        let mut lines = Vec::new();
        for i in 0..errors {
            lines.push(
                serde_json::json!({
                    "reason": "compiler-message",
                    "message": {
                        "level": "error",
                        "code": null,
                        "message": format!("error {i}"),
                        "spans": [],
                        "children": []
                    }
                })
                .to_string(),
            );
        }
        for i in 0..warnings {
            lines.push(
                serde_json::json!({
                    "reason": "compiler-message",
                    "message": {
                        "level": "warning",
                        "code": null,
                        "message": format!("warning {i}"),
                        "spans": [],
                        "children": []
                    }
                })
                .to_string(),
            );
        }
        fs::write(&data_file, lines.join("\n")).unwrap();
        let script_body = format!("#!/bin/sh\ncat {}\nexit 1", data_file.display());
        fs::write(&script, script_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn with_state_dir_and_lock(dir: &std::path::Path) -> impl Drop {
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", dir) };
        // Return an RAII guard that clears the env var on drop.
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };
            }
        }
        Guard
    }

    #[test]
    fn agent_origin_refactor_run_rejects_non_cargo_command() {
        let dir = tempfile::tempdir().unwrap();

        let response = run(
            &RefactorRunParams {
                title: "agent command allowlist".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "sh".into(),
                    args: vec!["-c".into(), "true".into()],
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: Some(DispatchOrigin::Agent),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "step_failed");
        assert!(
            resp.error
                .as_deref()
                .unwrap_or_default()
                .contains("agent_command_not_allowed"),
            "got: {resp:?}"
        );
    }

    #[test]
    fn agent_origin_refactor_run_requires_touches_for_cargo_fmt() {
        let dir = tempfile::tempdir().unwrap();

        let response = run(
            &RefactorRunParams {
                title: "agent cargo fmt allowlist".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "cargo".into(),
                    args: vec!["fmt".into()],
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: Some(DispatchOrigin::Agent),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "step_failed");
        assert!(
            resp.error
                .as_deref()
                .unwrap_or_default()
                .contains("agent_command_requires_touches"),
            "got: {resp:?}"
        );
    }

    #[test]
    fn agent_origin_refactor_run_allows_cargo_check_plan() {
        let dir = tempfile::tempdir().unwrap();

        let response = run(
            &RefactorRunParams {
                title: "agent cargo check allowlist".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "cargo".into(),
                    args: vec!["check".into(), "--message-format=json".into()],
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(true),
                    capture: Some(CaptureSpec::RustcJson),
                    on_failure: Some(OnFailure::ContinueForRepair),
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: Some(DispatchOrigin::Agent),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "planned");
        assert_eq!(resp.steps[0].status, "planned");
    }

    #[test]
    fn agent_origin_refactor_run_allows_cargo_fmt_with_touches_plan() {
        let dir = tempfile::tempdir().unwrap();

        let response = run(
            &RefactorRunParams {
                title: "agent cargo fmt allowlist with touches".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "cargo".into(),
                    args: vec!["fmt".into()],
                    cwd: None,
                    touches: vec!["src/lib.rs".into()],
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(false),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: Some(DispatchOrigin::Agent),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "planned");
        assert_eq!(resp.steps[0].status, "planned");
    }

    #[test]
    fn agent_origin_allowlist_failure_rolls_back_prior_writes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "agent allowlist rollback".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        optional: false,
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            module_name: Some("newmod".into()),
                            ..Default::default()
                        },
                    },
                    RefactorRunStep::Command {
                        command: "sh".into(),
                        args: vec!["-c".into(), "true".into()],
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: Some(DispatchOrigin::Agent),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "step_failed");
        assert!(
            resp.rolled_back,
            "allowlist failure should roll back: {response}"
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    // ── Gate A: legacy required: bool behaves identically when on_failure unset ──

    #[test]
    fn legacy_required_true_on_failure_unset_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());

        let response = run(
            &RefactorRunParams {
                title: "legacy required=true".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "false".into(),
                    args: Vec::new(),
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "step_failed");
        assert_eq!(resp.steps[0].status, "failed");
        assert!(resp.obligations.is_empty());
    }

    #[test]
    fn legacy_required_false_on_failure_unset_continues() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());

        let response = run(
            &RefactorRunParams {
                title: "legacy required=false".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "false".into(),
                    args: Vec::new(),
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(false),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            resp.status, "ok",
            "optional failure should not abort: {response}"
        );
        assert_eq!(resp.steps[0].status, "failed_optional");
        assert!(resp.obligations.is_empty());
    }

    // ── Gate B: ContinueForRepair obligation lifecycle ──

    #[test]
    fn continue_for_repair_consumed_commits() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());
        let script = make_failing_capture_script(dir.path(), "cargo_check", 2, 1);

        let response = run(
            &RefactorRunParams {
                title: "continue_for_repair consumed".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    // Step 0: soft-fail command that opens an obligation
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    // Step 1: stub plan kind that marks the obligation Consumed
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "test_consume_obligation".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    // Step 2: final required check passes
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            resp.status, "ok",
            "consumed obligation should commit: {response}"
        );
        assert!(!resp.rolled_back);
        assert_eq!(resp.steps[0].status, "soft_failed");
        assert_eq!(resp.steps[1].status, "ok");
        assert_eq!(resp.steps[2].status, "ok");
        assert_eq!(resp.obligations.len(), 1);
        assert_eq!(resp.obligations[0].status, "consumed");
        assert_eq!(resp.obligations[0].leftover_count, 0);
    }

    #[test]
    fn continue_for_repair_leftover_commits() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());
        let script = make_failing_capture_script(dir.path(), "cargo_check", 2, 0);

        let response = run(
            &RefactorRunParams {
                title: "continue_for_repair leftover".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "test_leftover_obligation".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            resp.status, "ok",
            "leftover obligation should commit: {response}"
        );
        assert!(!resp.rolled_back);
        assert_eq!(resp.obligations.len(), 1);
        assert_eq!(resp.obligations[0].status, "left_over");
        assert_eq!(resp.obligations[0].leftover_count, 1);
    }

    #[test]
    fn continue_for_repair_open_obligation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());
        let script = make_failing_capture_script(dir.path(), "cargo_check", 1, 0);

        // No stub consumption step — obligation stays Open.
        let response = run(
            &RefactorRunParams {
                title: "open obligation should fail".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    // No consume step here — obligation remains Open.
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "obligations_unresolved", "{response}");
        assert!(resp.rolled_back, "should have rolled back");
        assert_eq!(resp.obligations.len(), 1);
        assert_eq!(resp.obligations[0].status, "open");
    }

    #[test]
    fn continue_for_repair_final_check_fails_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());
        let script = make_failing_capture_script(dir.path(), "cargo_check", 1, 0);
        // Create a touch file that the soft-fail step will snapshot.
        let touch_file = dir.path().join("side_effect.txt");
        fs::write(&touch_file, b"before").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "final check fails rolls back".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: vec!["side_effect.txt".into()],
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "test_consume_obligation".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    // Final check fails — triggers rollback from soft-fail cursor.
                    RefactorRunStep::Command {
                        command: "false".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "step_failed", "{response}");
        assert!(
            resp.rolled_back,
            "should have rolled back from soft-fail cursor"
        );
        // The touch file should be restored to "before" (soft-fail cursor rollback).
        let content = fs::read_to_string(&touch_file).unwrap();
        assert_eq!(
            content, "before",
            "touch file should be restored on rollback"
        );
    }

    // ── Gate C: multi-soft-fail cursor stays at FIRST soft-fail ──

    #[test]
    fn multi_soft_fail_cursor_at_first_step() {
        // Two soft-fail commands. First is consumed; second stays Open.
        // Terminal commit fails. Rollback must go to the FIRST cursor,
        // not the second, because consumed obligations do not release their cursor.
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir_and_lock(state_dir.path());

        let script1 = make_failing_capture_script(dir.path(), "check1", 1, 0);
        let script2 = make_failing_capture_script(dir.path(), "check2", 1, 0);

        // Side-effect files: one touched by step1 (before soft-fail cursor),
        // one used as a touch on the first soft-fail step (inside cursor),
        // one touched by the second soft-fail step.
        let touch1 = dir.path().join("touch1.txt");
        let touch2 = dir.path().join("touch2.txt");
        fs::write(&touch1, b"t1_before").unwrap();
        fs::write(&touch2, b"t2_before").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "multi soft-fail cursor test".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    // Step 0: first soft-fail (touches touch1 → cursor = 0)
                    RefactorRunStep::Command {
                        command: path_string(&script1),
                        args: Vec::new(),
                        cwd: None,
                        touches: vec!["touch1.txt".into()],
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    // Step 1: consume the first obligation
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "test_consume_obligation".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    // Step 2: second soft-fail (touches touch2 → second cursor NOT set,
                    // first cursor stays live)
                    RefactorRunStep::Command {
                        command: path_string(&script2),
                        args: Vec::new(),
                        cwd: None,
                        touches: vec!["touch2.txt".into()],
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    // Step 3: final check passes — but second obligation is Open → fails.
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "obligations_unresolved", "{response}");
        assert!(resp.rolled_back);
        assert_eq!(resp.obligations.len(), 2);
        // First obligation: consumed.
        assert_eq!(resp.obligations[0].status, "consumed");
        // Second obligation: still open.
        assert_eq!(resp.obligations[1].status, "open");
        // Both touch files should be restored because rollback goes from cursor=0.
        let t1 = fs::read_to_string(&touch1).unwrap();
        let t2 = fs::read_to_string(&touch2).unwrap();
        assert_eq!(
            t1, "t1_before",
            "touch1 should be restored (inside first cursor)"
        );
        assert_eq!(
            t2, "t2_before",
            "touch2 should be restored (rollback from first cursor)"
        );
    }
}

#[cfg(test)]
mod rx_f2a_capture_tests {
    use super::*;
    use std::fs;

    fn project_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path).unwrap().display().to_string(),
            registered_at: "2026-05-09T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
        }
    }

    /// Build a minimal `cargo --message-format=json` line for one diagnostic.
    fn make_compiler_message(level: &str, msg: &str) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": level,
                "code": null,
                "message": msg,
                "spans": [],
                "children": []
            }
        })
        .to_string()
    }

    #[test]
    fn parse_rustc_json_correct_counts() {
        // 2 errors + 3 warnings
        let lines: Vec<String> = [
            make_compiler_message("error", "undefined variable"),
            make_compiler_message("error", "type mismatch"),
            make_compiler_message("warning", "unused import"),
            make_compiler_message("warning", "dead code"),
            make_compiler_message("warning", "unused variable"),
        ]
        .into_iter()
        .collect();
        let stdout = lines.join("\n").into_bytes();
        let diags = parse_rustc_json_output(&stdout);
        assert_eq!(diags.len(), 5);
        let errors = diags.iter().filter(|d| d.level == "error").count();
        let warnings = diags.iter().filter(|d| d.level == "warning").count();
        assert_eq!(errors, 2);
        assert_eq!(warnings, 3);
    }

    #[test]
    fn parse_rustc_json_tolerates_malformed_lines() {
        let mut lines = vec![
            "not json at all".to_string(),
            "{broken json".to_string(),
            make_compiler_message("error", "real error"),
            "".to_string(),
            make_compiler_message("warning", "real warning"),
        ];
        // Also include a valid JSON line that is NOT a compiler-message.
        lines.push(serde_json::json!({"reason": "build-finished", "success": true}).to_string());
        let stdout = lines.join("\n").into_bytes();
        let diags = parse_rustc_json_output(&stdout);
        // Only the two compiler-message lines should survive.
        assert_eq!(diags.len(), 2, "expected 2 diagnostics, got: {diags:?}");
        assert_eq!(diags.iter().filter(|d| d.level == "error").count(), 1);
        assert_eq!(diags.iter().filter(|d| d.level == "warning").count(), 1);
    }

    #[test]
    fn run_step_with_capture_populates_summary() {
        // Write a data file with 2 errors + 3 warnings, and a script that cats it.
        // Using a separate data file avoids shell quoting issues with JSON content.
        let dir = tempfile::tempdir().unwrap();
        let data_file = dir.path().join("cargo_output.txt");
        let script = dir.path().join("fake_cargo.sh");
        let output_lines: Vec<String> = [
            make_compiler_message("error", "e1"),
            make_compiler_message("error", "e2"),
            make_compiler_message("warning", "w1"),
            make_compiler_message("warning", "w2"),
            make_compiler_message("warning", "w3"),
        ]
        .into_iter()
        .collect();
        fs::write(&data_file, output_lines.join("\n")).unwrap();
        let script_body = format!("#!/bin/sh\ncat {}", data_file.display());
        fs::write(&script, script_body).unwrap();
        // chmod +x
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", state_dir.path()) };

        let response = run(
            &RefactorRunParams {
                title: "capture test".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: path_string(&script),
                    args: Vec::new(),
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(false),
                    capture: Some(CaptureSpec::RustcJson),
                    on_failure: None,
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };

        let run_resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_resp.status, "ok", "run failed: {response}");
        assert_eq!(run_resp.steps.len(), 1);
        let step = &run_resp.steps[0];
        let summary = step
            .captured_diagnostics_summary
            .as_ref()
            .expect("expected captured_diagnostics_summary on the command step");
        assert_eq!(summary.count, 5);
        assert_eq!(
            summary.severity_counts.get("error").copied().unwrap_or(0),
            2
        );
        assert_eq!(
            summary.severity_counts.get("warning").copied().unwrap_or(0),
            3
        );
    }

    #[test]
    fn run_step_without_capture_has_none_summary() {
        let dir = tempfile::tempdir().unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", state_dir.path()) };

        let response = run(
            &RefactorRunParams {
                title: "no capture test".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "true".into(),
                    args: Vec::new(),
                    cwd: None,
                    touches: Vec::new(),
                    required: Some(true),
                    capture: None,
                    on_failure: None,
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };

        let run_resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_resp.status, "ok");
        assert_eq!(run_resp.steps.len(), 1);
        assert!(
            run_resp.steps[0].captured_diagnostics_summary.is_none(),
            "expected None when capture=None"
        );
    }
}

#[cfg(test)]
mod rx_a1_deep_tests {
    use super::*;
    use std::{fs, path::Path};

    fn fixture_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/refactor/test_fixtures/rx_a1b_copy")
    }

    /// A1b: manifest-driven borrow_context classification for every fixture.
    #[test]
    fn borrow_context_manifest_fixtures() {
        let dir = fixture_dir();
        let manifest = fs::read_to_string(dir.join("manifest.txt")).unwrap();

        for raw_line in manifest.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Each line: fixture_file.rs: field_name: borrow_context: field_type
            let parts: Vec<&str> = line.splitn(4, ": ").collect();
            assert_eq!(parts.len(), 4, "bad manifest line: {line}");
            let (fixture_file, field_name, expected_ctx) = (parts[0], parts[1], parts[2]);

            let fixture_path = dir.join(fixture_file);

            let parsed = parse_rust_file(&fixture_path)
                .unwrap_or_else(|e| panic!("parse failed for {fixture_file}: {e}"));
            let methods = rust_impl_methods(&parsed);
            assert_eq!(
                methods.len(),
                1,
                "expected exactly 1 method in {fixture_file}, got {}",
                methods.len()
            );
            let impl_name = methods[0].impl_name.clone();
            let method_name = methods[0].item.name.clone().unwrap_or_default();

            let deep = rust_deep::deep_analyze_extract(&fixture_path, &impl_name, &[&method_name])
                .unwrap_or_else(|e| panic!("deep_analyze_extract failed for {fixture_file}: {e}"));

            let field = deep
                .captured_self_fields
                .iter()
                .find(|f| f.field_name == field_name)
                .unwrap_or_else(|| {
                    let names: Vec<_> = deep
                        .captured_self_fields
                        .iter()
                        .map(|f| &f.field_name)
                        .collect();
                    panic!("field `{field_name}` not found in {fixture_file}; got: {names:?}")
                });

            assert_eq!(
                field.borrow_context, expected_ctx,
                "wrong borrow_context for field `{field_name}` in {fixture_file}"
            );
        }
    }

    /// A1c: self.method() and Self::method() calls that are NOT in the extraction
    /// set are reported as unresolved_callbacks.
    #[test]
    fn unresolved_callbacks_self_and_static() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("service.rs");
        fs::write(
            &src,
            r#"struct Service {
    value: u32,
}
impl Service {
    fn compute(&self) -> u32 {
        let h = self.helper();
        let b = Self::build();
        h + b + self.value
    }
    fn helper(&self) -> u32 { 0 }
    fn build() -> u32 { 1 }
}
"#,
        )
        .unwrap();

        let deep = rust_deep::deep_analyze_extract(&src, "impl Service", &["compute"]).unwrap();

        let callees: Vec<&str> = deep
            .unresolved_callbacks
            .iter()
            .map(|c| c.callee.as_str())
            .collect();
        assert!(
            callees.contains(&"self.helper"),
            "expected self.helper in callbacks; got {callees:?}"
        );
        assert!(
            callees.contains(&"Self::build"),
            "expected Self::build in callbacks; got {callees:?}"
        );
    }

    /// A1d: lifetime parameters from impl<'a> travel with the extracted methods.
    #[test]
    fn captured_lifetimes_from_impl_type_parameters() {
        let dir = fixture_dir();
        let src = dir.join("fixture_copy_whitelist_ref.rs");

        let parsed = parse_rust_file(&src).unwrap();
        let methods = rust_impl_methods(&parsed);
        assert_eq!(methods.len(), 1);
        let impl_name = methods[0].impl_name.clone();
        let method_name = methods[0].item.name.clone().unwrap_or_default();

        let deep = rust_deep::deep_analyze_extract(&src, &impl_name, &[&method_name]).unwrap();

        assert!(
            deep.captured_lifetimes.contains(&"'a".to_string()),
            "expected 'a in captured_lifetimes; got {:?}",
            deep.captured_lifetimes
        );
    }

    /// A1a wiring: plan_extract_rust_impl_methods with deep_analysis=true sets
    /// semantic_status=IndexedHints and populates deep_analysis on the plan.
    #[test]
    fn plan_extract_rust_impl_methods_with_deep_analysis() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rs");
        let tgt = dir.path().join("tgt.rs");

        fs::write(
            &src,
            r#"use std::collections::HashMap;
struct Cache {
    map: HashMap<String, String>,
}
impl Cache {
    pub fn insert(&mut self, k: String, v: String) {
        self.map.insert(k, v);
    }
}
"#,
        )
        .unwrap();
        fs::write(&tgt, "").unwrap();

        let plan_json = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&src),
            target: Some(path_string(&tgt)),
            item_names: Some(vec!["insert".into()]),
            deep_analysis: Some(true),
            ..Default::default()
        })
        .unwrap();

        let p: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(
            p.semantic_status,
            SemanticStatus::IndexedHints,
            "expected IndexedHints when deep_analysis=true"
        );
        let da = p
            .deep_analysis
            .as_ref()
            .expect("deep_analysis should be present on the plan");
        let field = da
            .captured_self_fields
            .iter()
            .find(|f| f.field_name == "map")
            .expect("expected captured field `map`");
        assert_eq!(
            field.borrow_context, "unique_ref",
            "HashMap accessed via &mut self should be unique_ref"
        );
    }
}

#[cfg(test)]
mod rx_a2_fixme_marker_tests {
    use super::*;
    use std::{fs, path::Path};

    fn fixture_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/refactor/test_fixtures/rx_a1b_copy")
    }

    /// Source with a unique_ref field — guarantees at least one blocking finding.
    fn unique_ref_source() -> &'static str {
        r#"use std::collections::HashMap;
struct Cache {
    map: HashMap<String, String>,
}
impl Cache {
    pub fn get(&self, k: &str) -> Option<&String> {
        self.map.get(k)
    }
}
"#
    }

    /// A2-1: plan with a captured_self_field → plan_status=Blocked, fixme_count.plan_only ≥ 1.
    #[test]
    fn plan_with_captured_field_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rs");
        let tgt = dir.path().join("tgt.rs");
        fs::write(&src, unique_ref_source()).unwrap();
        fs::write(&tgt, "").unwrap();

        let plan_json = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&src),
            target: Some(path_string(&tgt)),
            item_names: Some(vec!["get".into()]),
            deep_analysis: Some(true),
            ..Default::default()
        })
        .unwrap();

        let p: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(p.plan_status, PlanStatus::Blocked, "expected Blocked plan");
        let fc = p
            .fixme_count
            .as_ref()
            .expect("fixme_count should be present on Blocked plan");
        assert!(
            fc.plan_only >= 1,
            "expected fixme_count.plan_only ≥ 1, got {}",
            fc.plan_only
        );
    }

    /// A2-2: apply attempt on a Blocked plan returns error "plan_blocked"; no files written.
    #[test]
    fn apply_blocked_plan_returns_error_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rs");
        let tgt = dir.path().join("tgt.rs");
        fs::write(&src, unique_ref_source()).unwrap();
        fs::write(&tgt, "").unwrap();
        let original_src = fs::read_to_string(&src).unwrap();

        let plan_json = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&src),
            target: Some(path_string(&tgt)),
            item_names: Some(vec!["get".into()]),
            deep_analysis: Some(true),
            ..Default::default()
        })
        .unwrap();

        let p: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(p.plan_status, PlanStatus::Blocked);

        let project = tempfile::tempdir().unwrap();
        let err = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(p).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
                cwd: None,
                force_path: None,
            },
            &[ProjectRecord {
                project_id: "test".into(),
                repo_id: None,
                canonical_path: project.path().to_string_lossy().into_owned(),
                registered_at: "2026-01-01T00:00:00Z".into(),
                is_git_repo: false,
                languages: Default::default(),
            }],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("plan_blocked"),
            "expected plan_blocked in error, got: {err}"
        );
        assert_eq!(
            fs::read_to_string(&src).unwrap(),
            original_src,
            "source file must not be modified by a blocked plan"
        );
    }

    /// A2-3: target FileEdit new_text contains a FIXME(refactor-plan-only) marker.
    #[test]
    fn blocked_plan_file_edit_new_text_contains_fixme_marker() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rs");
        let tgt = dir.path().join("tgt.rs");
        fs::write(&src, unique_ref_source()).unwrap();
        fs::write(&tgt, "").unwrap();

        let plan_json = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&src),
            target: Some(path_string(&tgt)),
            item_names: Some(vec!["get".into()]),
            deep_analysis: Some(true),
            ..Default::default()
        })
        .unwrap();

        let p: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let target_edit = p
            .edits
            .iter()
            .find(|e| e.path == path_string(&tgt))
            .expect("target FileEdit not found");
        let new_text = target_edit
            .new_text
            .as_deref()
            .expect("new_text should be Some on the target FileEdit of a Blocked plan");
        assert!(
            new_text.contains("FIXME(refactor-plan-only)"),
            "expected FIXME(refactor-plan-only) in new_text, got: {new_text:.200}"
        );
    }

    /// A2-4: plan without deep_analysis stays Planned and applies cleanly (no FIXME in worktree).
    #[test]
    fn clean_plan_applies_without_fixme_markers() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rs");
        let tgt = dir.path().join("tgt.rs");
        fs::write(&src, unique_ref_source()).unwrap();
        fs::write(&tgt, "").unwrap();

        let plan_json = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&src),
            target: Some(path_string(&tgt)),
            item_names: Some(vec!["get".into()]),
            deep_analysis: None,
            ..Default::default()
        })
        .unwrap();

        let p: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(
            p.plan_status,
            PlanStatus::Planned,
            "expected Planned (no deep_analysis)"
        );

        apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(p).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
                cwd: None,
                force_path: None,
            },
            &[ProjectRecord {
                project_id: "test".into(),
                repo_id: None,
                canonical_path: dir.path().to_string_lossy().into_owned(),
                registered_at: "2026-01-01T00:00:00Z".into(),
                is_git_repo: false,
                languages: Default::default(),
            }],
        )
        .unwrap();

        let tgt_content = fs::read_to_string(&tgt).unwrap();
        assert!(
            !tgt_content.contains("FIXME(refactor-plan-only)"),
            "FIXME markers must not appear in applied target file"
        );
        let src_content = fs::read_to_string(&src).unwrap();
        assert!(
            !src_content.contains("FIXME(refactor-plan-only)"),
            "FIXME markers must not appear in applied source file"
        );
    }

    /// A2-5: for each rx_a1b_copy fixture, generate_fixme_markers count equals total findings.
    #[test]
    fn fixture_loop_marker_count_equals_findings() {
        let dir = fixture_dir();
        let fixtures: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rs"))
            .collect();
        assert!(
            !fixtures.is_empty(),
            "no fixture files found in rx_a1b_copy"
        );

        for entry in fixtures {
            let fixture_path = entry.path();
            let fixture_name = fixture_path.file_name().unwrap().to_string_lossy();

            let parsed = parse_rust_file(&fixture_path)
                .unwrap_or_else(|e| panic!("parse failed for {fixture_name}: {e}"));
            let methods = rust_impl_methods(&parsed);
            assert!(!methods.is_empty(), "no methods in {fixture_name}");

            let impl_name = methods[0].impl_name.clone();
            let method_name = methods[0].item.name.clone().unwrap_or_default();

            let da =
                rust_deep::deep_analyze_extract(&fixture_path, &impl_name, &[method_name.as_str()])
                    .unwrap_or_else(|e| panic!("deep analysis failed for {fixture_name}: {e}"));

            let expected_count = da.captured_self_fields.len()
                + da.unresolved_callbacks.len()
                + da.captured_lifetimes.len()
                + da.inherited_generics.len();

            let (_markers, count) = rust_deep::generate_fixme_markers(&da);
            assert_eq!(
                count, expected_count,
                "fixture {fixture_name}: marker count {count} != findings count {expected_count}"
            );
        }
    }

    fn g15_project_record(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "g15-test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            registered_at: "2026-05-12T00:00:00Z".to_string(),
            is_git_repo: true,
            languages: Default::default(),
        }
    }

    // G15: bbox_refactor_apply refuses to apply when the caller's cwd
    // is in a different git toplevel than the plan's recorded paths,
    // unless `force_path=true`. Without the guard, plans built against
    // the main checkout silently contaminate the main tree when the
    // operator's session had switched into a worktree between plan and
    // apply.
    #[test]
    fn g15_apply_refuses_cross_worktree_without_force_path() {
        // Two separate temp dirs simulate plan-toplevel vs cwd-toplevel.
        let plan_repo = tempfile::tempdir().unwrap();
        let cwd_repo = tempfile::tempdir().unwrap();
        for repo in [plan_repo.path(), cwd_repo.path()] {
            std::process::Command::new("git")
                .arg("init")
                .arg("-q")
                .current_dir(repo)
                .status()
                .expect("git init");
        }
        // A plan edit anchored against plan_repo. Build the plan via
        // serde_json so we don't have to track every field on RefactorPlan.
        let plan_file = plan_repo.path().join("a.rs");
        let original = b"fn old() {}\n";
        fs::write(&plan_file, original).unwrap();
        let plan_value = serde_json::json!({
            "title": "g15-test",
            "kind": "replace_text",
            "semantic_status": "syntax_only",
            "dry_run": false,
            "edits": [{
                "path": plan_file.to_string_lossy(),
                "original_sha256": sha256_hex(original),
                "edits": [{
                    "byte_start": 3,
                    "byte_end": 6,
                    "replacement": "new"
                }]
            }],
            "validations": [],
            "items": [],
        });

        // cwd in a different repo — should refuse.
        let result = apply(
            &RefactorApplyParams {
                plan: plan_value.clone(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
                cwd: Some(cwd_repo.path().to_string_lossy().into_owned()),
                force_path: None,
            },
            &[g15_project_record(plan_repo.path())],
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cross_worktree_apply"),
            "expected cross_worktree_apply refusal, got: {err}"
        );

        // Same call with force_path=true bypasses the G15 guard.
        // We don't assert the apply succeeds (it may fail later on a
        // sha mismatch or validation), only that the G15 refusal
        // doesn't fire.
        let result_force = apply(
            &RefactorApplyParams {
                plan: plan_value,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
                cwd: Some(cwd_repo.path().to_string_lossy().into_owned()),
                force_path: Some(true),
            },
            &[g15_project_record(plan_repo.path())],
        );
        if let Err(err) = &result_force {
            assert!(
                !err.to_string().contains("cross_worktree_apply"),
                "force_path=true must bypass G15 refusal, got: {err}"
            );
        }
    }

    // G15: when cwd's git toplevel matches the plan's, apply proceeds
    // without the cross_worktree refusal (sanity for the happy path).
    #[test]
    fn g15_apply_proceeds_when_cwd_matches_plan_toplevel() {
        let repo = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(repo.path())
            .status()
            .expect("git init");
        let file = repo.path().join("a.rs");
        let original = b"fn old() {}\n";
        fs::write(&file, original).unwrap();
        let plan_value = serde_json::json!({
            "title": "g15-match",
            "kind": "replace_text",
            "semantic_status": "syntax_only",
            "dry_run": false,
            "edits": [{
                "path": file.to_string_lossy(),
                "original_sha256": sha256_hex(original),
                "edits": [{
                    "byte_start": 3,
                    "byte_end": 6,
                    "replacement": "new"
                }]
            }],
            "validations": [],
            "items": [],
        });

        let result = apply(
            &RefactorApplyParams {
                plan: plan_value,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
                cwd: Some(repo.path().to_string_lossy().into_owned()),
                force_path: None,
            },
            &[g15_project_record(repo.path())],
        );
        // The G15 guard must not fire when cwd's toplevel matches
        // the plan's. Apply may still fail at a later validation
        // step in this minimal fixture; we assert only the G15 check.
        if let Err(err) = &result {
            assert!(
                !err.to_string().contains("cross_worktree_apply"),
                "matching toplevels must not trigger G15 refusal, got: {err}"
            );
        }
    }

    #[test]
    fn rust_top_level_dependency_analysis_reports_edges_and_external_refs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let lib = src.join("lib.rs");
        fs::write(
            &lib,
            "pub struct Config;\n\nconst LIMIT: usize = 3;\n\nfn helper() -> Config {\n    Config\n}\n\npub fn parse() -> usize {\n    let _cfg: Config = helper();\n    LIMIT\n}\n",
        )
        .unwrap();
        fs::write(
            src.join("sibling.rs"),
            "use crate::{parse, Config};\n\nfn call() {\n    let _ = parse();\n    let _cfg: Option<Config> = None;\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rust_top_level_dependency_analysis".into(),
            source: path_string(&lib),
            project_dir: Some(path_string(dir.path())),
            item_names: Some(vec![
                "Config".into(),
                "LIMIT".into(),
                "helper".into(),
                "parse".into(),
            ]),
            item_kinds: None,
            ..Default::default()
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(value["kind"], "rust_top_level_dependency_analysis");
        assert_eq!(value["dry_run"], true);
        assert!(value["edits"].as_array().unwrap().is_empty());

        let graph = &value["top_level_dependency_graph"];
        let edges = graph["edges"].as_array().unwrap();
        assert!(edges.iter().any(|edge| {
            edge["from"] == "parse" && edge["to"] == "helper" && edge["kind"] == "calls"
        }));
        assert!(edges.iter().any(|edge| {
            edge["from"] == "parse" && edge["to"] == "Config" && edge["kind"] == "type_ref"
        }));
        assert!(edges.iter().any(|edge| {
            edge["from"] == "parse" && edge["to"] == "LIMIT" && edge["kind"] == "global_ref"
        }));

        let external_refs = graph["external_references"].as_array().unwrap();
        assert!(external_refs.iter().any(|reference| {
            reference["item"] == "parse"
                && reference["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("sibling.rs"))
        }));
        assert!(external_refs.iter().any(|reference| {
            reference["item"] == "Config"
                && reference["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("sibling.rs"))
        }));
    }

    #[test]
    fn rust_top_level_dependency_analysis_warns_on_macro_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib.rs");
        fs::write(
            &lib,
            "fn macro_user() {\n    println!(\"opaque {}\", 1);\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rust_top_level_dependency_analysis".into(),
            source: path_string(&lib),
            item_names: Some(vec!["macro_user".into()]),
            ..Default::default()
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let warnings = value["top_level_dependency_graph"]["warnings"]
            .as_array()
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap_or_default().contains("macro_user"))
        );
    }
}
