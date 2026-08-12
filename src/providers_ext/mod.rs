//! Daemon-side entity providers — the orchestration-flavored half of the
//! provider registry. The corpus providers live in `crate::providers`
//! (the bbox-providers crate); the providers here read daemon state
//! (`TaskStore`, brofiles) through the context's opaque `ext` slot and are
//! handed to `providers::register_extra_providers` at SharedState
//! construction.
mod brofile;
mod project_graph;
mod virtual_task;

use crate::providers::InspectableEntityProvider;

pub(crate) fn extra_providers() -> Vec<Box<dyn InspectableEntityProvider>> {
    vec![
        Box::new(virtual_task::TaskProvider),
        Box::new(brofile::BrofileProvider),
        Box::new(project_graph::ProjectGraphVertexProvider::published()),
        Box::new(project_graph::ProjectGraphVertexProvider::provisional()),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::mcp_tools::ref_size::{
        FileInputResolution, RefSizeParams, ValidatedFileInput, ValidatedFileInputs, ref_size,
        ref_size_with_validated_files,
    };
    use crate::providers::{ProviderContext, all_providers, provider_for};

    use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

    fn full_registry_ctx() -> ProviderContext<'static> {
        crate::init_system_memory_for_tests();
        crate::providers::register_extra_providers(super::extra_providers());
        ProviderContext::empty_for_tests()
    }

    fn sample_refs() -> Vec<&'static str> {
        vec![
            "knowledge:abc12345",
            "system_memory:sm-agentic-opening-sequence",
            "file:README.md",
            "project_file:proj1234:relhash:chunkhash:0",
            "transcript:claude:session123:42:0",
            "session:claude:session123",
            "thread:thread-12345678",
            "note:note-12345678",
            "symbol:proj1234:crate::Type::method:defhash",
            "brofile:auditor",
            "whiteboard:board-12345678",
            "commit:repo1234:abcdef1234567890",
            "task:task-12345678",
            "bash_call:session123:7",
            "agent:code-reviewer@v3",
            "packet:domain:phase-decompose/triage",
            "artifact:packet/phase-decompose/triage@1",
        ]
    }

    #[test]
    fn registry_dispatches_every_entity_type() {
        let ctx = full_registry_ctx();
        for raw in sample_refs() {
            let parsed = EntityRef::parse(raw).unwrap();
            assert_eq!(parsed.render(), raw);
            let provider = provider_for(parsed.entity_type());
            assert!(provider.owns_ref(&parsed));
            assert_eq!(provider.handles_virtual(), parsed.is_virtual());
            let view = provider.get_entity(&ctx, &parsed).unwrap();
            assert_eq!(view.entity_type, parsed.entity_type());
        }
    }

    #[test]
    fn compact_labels_fit_inline_budget() {
        let ctx = full_registry_ctx();
        for raw in sample_refs() {
            let parsed = EntityRef::parse(raw).unwrap();
            let provider = provider_for(parsed.entity_type());
            let label = provider.compact_label(&ctx, &parsed).unwrap();
            assert!(label.len() <= 80, "{raw}: {label}");
        }
    }

    #[test]
    fn registry_covers_entity_type_enum() {
        crate::providers::register_extra_providers(super::extra_providers());
        crate::providers::register_extra_providers(super::extra_providers());
        for entity_type in EntityType::ALL {
            provider_for(entity_type);
        }
        assert_eq!(all_providers().len(), EntityType::ALL.len());
    }

    // ── relocated from mcp_tools/ref_size.rs: these exercise ref_size
    // against daemon state (SharedState::for_test), which the peeled
    // mcp-tools crate must not name. ──
    #[test]
    fn virtual_entity_ref_measures_provider_properties_json() {
        crate::providers::register_extra_providers(super::extra_providers());
        let ctx = crate::providers::ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: vec!["task:task-123".into()],
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert!(value["total_bytes"].as_u64().unwrap() > 0);
        assert_eq!(value["per_ref"][0]["ref"], "task:task-123");
        assert_eq!(value["per_ref"][0]["entity_type"], "task");
        assert_eq!(value["per_ref"][0]["source"], "entity_properties_json");
    }

    #[test]
    fn file_ref_measures_registered_project_file_content() {
        let store = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("docs")).unwrap();
        let file_path = project.path().join("docs/design.md");
        fs::write(&file_path, "hello\nworld\n").unwrap();

        let stores = crate::server::state::SharedState::for_test(store.path());
        stores
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(project.path())
            .unwrap();
        let ctx = crate::providers::ProviderContext::new_with_ext(stores.corpus_stores(), &stores);
        let files = ValidatedFileInputs::from([(
            "docs/design.md".into(),
            FileInputResolution::Validated(ValidatedFileInput {
                bytes: std::fs::metadata(&file_path).unwrap().len(),
            }),
        )]);
        let out = ref_size_with_validated_files(
            &RefSizeParams {
                refs: vec!["file:docs/design.md".into()],
                project_dir: None,
            },
            &ctx,
            &files,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_bytes"], 12);
        assert_eq!(value["per_ref"][0]["ref"], "file:docs/design.md");
        assert_eq!(value["per_ref"][0]["entity_type"], "file");
        assert_eq!(value["per_ref"][0]["source"], "file_content");
    }

    #[test]
    fn file_ref_uses_only_caller_validated_worktree_file() {
        let store = tempfile::tempdir().unwrap();
        let registered = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        fs::create_dir_all(registered.path().join("scripts")).unwrap();
        fs::create_dir_all(worktree.path().join("scripts")).unwrap();
        fs::write(registered.path().join("scripts/guard.py"), "old").unwrap();
        fs::write(worktree.path().join("scripts/guard.py"), "new guard\n").unwrap();

        let stores = crate::server::state::SharedState::for_test(store.path());
        stores
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(registered.path())
            .unwrap();
        let ctx = crate::providers::ProviderContext::new_with_ext(stores.corpus_stores(), &stores);
        let files = ValidatedFileInputs::from([(
            "scripts/guard.py".into(),
            FileInputResolution::Validated(ValidatedFileInput {
                bytes: std::fs::metadata(worktree.path().join("scripts/guard.py"))
                    .unwrap()
                    .len(),
            }),
        )]);
        let out = ref_size_with_validated_files(
            &RefSizeParams {
                refs: vec!["file:scripts/guard.py".into()],
                project_dir: Some(worktree.path().to_string_lossy().into_owned()),
            },
            &ctx,
            &files,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_bytes"], 10);
        assert_eq!(value["per_ref"][0]["ref"], "file:scripts/guard.py");
        assert_eq!(value["per_ref"][0]["source"], "file_content");
    }
}
