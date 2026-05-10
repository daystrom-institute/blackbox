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
        assert!(chunk
            .excerpt
            .as_deref()
            .unwrap_or_default()
            .contains("alpha"));
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
        assert!(parsed
            .items
            .iter()
            .any(|item| item.kind == "struct_item" && item.name.as_deref() == Some("Thing")));
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
        assert!(method
            .attributes
            .iter()
            .any(|attr| attr == "#[tool(description = \"x\")]"));
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("async fn move_me"));
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&target)
            .unwrap()
            .starts_with("/*!\nmodule docs\n*/\n\nuse super::*;"));
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
        assert!(err
            .to_string()
            .contains("only supports item_kinds impl_method"));
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
        assert!(leftovers
            .iter()
            .any(|leftover| leftover.as_str().unwrap().contains("keep_a")));
        assert!(!leftovers
            .iter()
            .any(|leftover| leftover.as_str().unwrap().contains("keep_b")));
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(run_response
            .error
            .unwrap()
            .contains("outside registered projects"));
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
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
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
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(run_response.steps[1]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("put arguments in args"));
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
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&source)
            .unwrap()
            .contains("impl Thing { pub(super) fn hidden(&self) {} }"));
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&source)
            .unwrap()
            .contains("impl Thing { pub(super) async fn hidden(&self) {} }"));
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
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&source).unwrap(), "pub mod packets;\n");
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
        assert!(parsed
            .items
            .iter()
            .any(|item| item.kind.contains("export") || item.name.as_deref() == Some("helper")));
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
            semantic_status: SemanticStatus::StructuralOnly,
            dry_run: true,
            file_moves: Vec::new(),
            edits: vec![FileEdit {
                path: path_string(&source),
                original_sha256: sha256_hex(b"fn f() {}\n"),
                edits: Vec::new(),
            }],
            validations: Vec::new(),
            items: Vec::new(),
            leftovers: Vec::new(),
            captured_variables: Vec::new(),
            remaining_source_accessors: Vec::new(),
            external_calls: Vec::new(),
            inherited_dependencies: Vec::new(),
        };
        let err = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
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
            semantic_status: SemanticStatus::StructuralOnly,
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
            }],
            validations: Vec::new(),
            items: Vec::new(),
            leftovers: Vec::new(),
            captured_variables: Vec::new(),
            remaining_source_accessors: Vec::new(),
            external_calls: Vec::new(),
            inherited_dependencies: Vec::new(),
        };

        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[],
        )
        .unwrap();

        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(source).unwrap(), "fn g() {}\n");
    }
}
