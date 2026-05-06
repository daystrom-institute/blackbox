#![allow(dead_code)]

pub mod commands;
pub mod events;
pub mod journal;
pub mod proposals;
pub mod queue;
pub mod registry;
pub mod types;

pub use journal::ActionJournal;
pub use proposals::ProposalStore;
pub use registry::BadgeyRegistry;

#[cfg(test)]
mod artifact_tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use crate::orchestration::agents::adapter::{
        AgentAdapterRegistry, AgentDispatchAdapter, AgentDispatchError, AgentDispatchResult,
        DispatchContext,
    };
    use crate::orchestration::agents::types::AgentArtifact;
    use crate::orchestration::agents::validate::{validate_agent_install, InstallCtx};
    use crate::orchestration::brofile::Brofile;
    use serde_json::Value;

    struct BadgeyTestAdapter;

    impl AgentDispatchAdapter for BadgeyTestAdapter {
        fn name(&self) -> &'static str {
            "badgey"
        }

        fn dispatch(
            &self,
            _manifest: &crate::orchestration::agents::types::AgentManifest,
            _args: Value,
            _ctx: DispatchContext,
        ) -> Pin<
            Box<dyn Future<Output = Result<AgentDispatchResult, AgentDispatchError>> + Send + '_>,
        > {
            Box::pin(async {
                Err(AgentDispatchError::AdapterFailed {
                    message: "not used in validation tests".to_string(),
                })
            })
        }
    }

    #[test]
    fn badgey_brofile_artifacts_parse_and_deny_bro_tools() {
        for raw in [
            include_str!("../../../examples/badgey/brofiles/badgey-persona.json"),
            include_str!("../../../examples/badgey/brofiles/badgey-scout-persona.json"),
        ] {
            let brofile: Brofile = serde_json::from_str(raw).unwrap();
            assert!(
                brofile
                    .filters
                    .as_ref()
                    .is_some_and(|filters| filters
                        .disallow
                        .contains(&"mcp__blackbox__bro_*".to_string()))
            );
            assert!(brofile.lens.as_deref().is_some_and(|lens| !lens.is_empty()));
        }
    }

    #[test]
    fn badgey_agent_artifacts_parse_and_reference_brofiles() {
        let badgey: AgentArtifact =
            serde_json::from_str(include_str!("../../../examples/badgey/agents/badgey.json"))
                .unwrap();
        assert_eq!(badgey.kind, "agent");
        assert_eq!(badgey.name, "badgey");
        assert_eq!(badgey.manifest.brofile_ref.as_deref(), Some("badgey-persona"));
        assert_eq!(badgey.manifest.dispatch_adapter.as_deref(), Some("badgey"));

        let scout: AgentArtifact = serde_json::from_str(include_str!(
            "../../../examples/badgey/agents/badgey-scout.json"
        ))
        .unwrap();
        assert_eq!(scout.name, "badgey-scout");
        assert_eq!(
            scout.manifest.brofile_ref.as_deref(),
            Some("badgey-scout-persona")
        );
        assert!(scout.manifest.dispatch_adapter.is_none());
    }

    #[test]
    fn badgey_agent_install_validation_is_adapter_gated() {
        let value: Value =
            serde_json::from_str(include_str!("../../../examples/badgey/agents/badgey.json"))
                .unwrap();
        let empty_registry = AgentAdapterRegistry::new();
        let ctx = InstallCtx {
            adapter_registry: &empty_registry,
            brofile_exists: |name| name == "badgey-persona",
            agent_exists: |_| false,
        };
        let err = validate_agent_install(&value, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_dispatch_adapter");

        let mut registry = AgentAdapterRegistry::new();
        registry.register(Arc::new(BadgeyTestAdapter));
        let ctx = InstallCtx {
            adapter_registry: &registry,
            brofile_exists: |name| name == "badgey-persona",
            agent_exists: |_| false,
        };
        validate_agent_install(&value, &ctx).unwrap();
    }

    #[test]
    fn badgey_scout_agent_install_validation_does_not_need_adapter() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../examples/badgey/agents/badgey-scout.json"
        ))
        .unwrap();
        let registry = AgentAdapterRegistry::new();
        let ctx = InstallCtx {
            adapter_registry: &registry,
            brofile_exists: |name| name == "badgey-scout-persona",
            agent_exists: |_| false,
        };
        validate_agent_install(&value, &ctx).unwrap();
    }
}
