//! Daemon-side entity providers — the orchestration-flavored half of the
//! provider registry. The corpus providers live in `crate::providers`
//! (the bbox-providers crate); the providers here read daemon state
//! (`TaskStore`, brofiles) through the context's opaque `ext` slot and are
//! handed to `providers::register_extra_providers` at SharedState
//! construction.
mod brofile;
mod virtual_task;

use crate::providers::InspectableEntityProvider;

pub(crate) fn extra_providers() -> Vec<Box<dyn InspectableEntityProvider>> {
    vec![
        Box::new(virtual_task::TaskProvider),
        Box::new(brofile::BrofileProvider),
    ]
}

#[cfg(test)]
mod tests {
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
        for entity_type in EntityType::ALL {
            provider_for(entity_type);
        }
        assert_eq!(all_providers().len(), EntityType::ALL.len());
    }
}
