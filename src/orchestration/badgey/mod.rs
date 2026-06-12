#![allow(dead_code)]

//! Badgey: the first configured consumer of the generic consultant runtime
//! (`orchestration::consultant`). This module keeps the Badgey-specific
//! vocabulary (thread events, wrapper commands) plus aliased re-exports of the
//! consultant core under the legacy `badgey::*` paths so consumer call sites
//! keep compiling during the dissolution.
//! See design/orchestration/agents/consultant-runtime.md.

pub mod commands;
pub mod events;
pub mod vocabulary;

pub use vocabulary::descriptor;

pub mod types {
    pub use super::vocabulary::ProposalKind;
    pub use crate::orchestration::consultant::types::*;
    pub use crate::orchestration::consultant::types::{
        ConsultantId as BadgeyId, ConsultantScope as BadgeyScope,
    };
}

pub mod registry {
    pub use crate::orchestration::consultant::registry::ConsultantInstance as BadgeyInstance;
}

pub mod queue {
    pub use crate::orchestration::consultant::queue::*;
}

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
    use crate::orchestration::agents::validate::{InstallCtx, validate_agent_install};
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
            include_str!("../../../system-defaults/badgey/brofiles/badgey-persona.json"),
            include_str!("../../../system-defaults/badgey/brofiles/badgey-scout-persona.json"),
        ] {
            let brofile: Brofile = serde_json::from_str(raw).unwrap();
            assert!(brofile.filters.as_ref().is_some_and(|filters| {
                filters
                    .disallow
                    .contains(&"mcp__blackbox__bro_*".to_string())
            }));
            assert!(brofile.lens.as_deref().is_some_and(|lens| !lens.is_empty()));
        }
    }

    #[test]
    fn badgey_agent_artifacts_parse_and_reference_brofiles() {
        let badgey: AgentArtifact = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/agents/badgey.json"
        ))
        .unwrap();
        assert_eq!(badgey.kind, "agent");
        assert_eq!(badgey.name, "badgey");
        assert_eq!(
            badgey.manifest.brofile_ref.as_deref(),
            Some("badgey-persona")
        );
        assert_eq!(badgey.manifest.dispatch_adapter.as_deref(), Some("badgey"));

        let scout: AgentArtifact = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/agents/badgey-scout.json"
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
        let value: Value = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/agents/badgey.json"
        ))
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
            "../../../system-defaults/badgey/agents/badgey-scout.json"
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

    #[test]
    fn badgey_iac_examples_parse() {
        for raw in [
            include_str!("../../../system-defaults/badgey/crons/badgey-triage-daily.json"),
            include_str!("../../../system-defaults/badgey/crons/badgey-close-loops-weekly.json"),
        ] {
            let cron: crate::crons::CronSpec = serde_json::from_str(raw).unwrap();
            assert!(cron.name.starts_with("badgey-"));
            assert_eq!(cron.concurrency, 1);
        }

        for raw in [
            include_str!("../../../system-defaults/badgey/workflows/badgey-eval-arc.json"),
            include_str!("../../../system-defaults/badgey/workflows/badgey-graduation-eval.json"),
        ] {
            let workflow: crate::workflow::Workflow = serde_json::from_str(raw).unwrap();
            assert!(workflow.name.starts_with("badgey-"));
            crate::workflow::compile(workflow).unwrap();
        }

        let packet: crate::packets::CompileParams = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/packets/badgey-self-eval.json"
        ))
        .unwrap();
        assert_eq!(packet.domain, "badgey/self-eval");
        let routing_packet: crate::packets::CompileParams = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/packets/badgey-cron-routing.json"
        ))
        .unwrap();
        assert_eq!(routing_packet.domain, "badgey/cron-routing");
    }
}
