//! Context shell for the macro planner (M3).
//!
//! [`MacroPlannerContext`] is the single struct the planner takes at call
//! time. It holds:
//!
//! - A boxed [`JavaMacroBackend`] (defaults to [`UnavailableBackend`] — fail
//!   closed until the Phase 3 sidecar lands).
//! - An optional [`LspSessionManager`] for probe execution via jdtls (unused
//!   until Phase 4 probe bindings).
//!
//! # Design note
//!
//! The backend is NOT placed on [`bbox_refactor::PlanContext`] — the macro
//! layer owns its own context so it can carry Java-specific state without
//! polluting the general refactor plumbing.

use bbox_lsp::LspSessionManager;
use crate::backend::{JavaMacroBackend, UnavailableBackend};
use crate::probe::{ProbeRunner, UnavailableProbeRunner};

// ---------------------------------------------------------------------------
// MacroPlannerContext
// ---------------------------------------------------------------------------

/// Context threaded through the macro planner at plan time.
///
/// Constructed by the MCP surface before invoking `macro_plan`; holds all
/// long-lived services the planner may need. Fields are `pub` so the planner
/// module (M3) can read them directly.
pub struct MacroPlannerContext {
    /// Java source generation / rewrite backend.
    ///
    /// Defaults to [`UnavailableBackend`] (fail-closed) until the
    /// OpenRewrite/JavaPoet sidecar is wired up in Phase 3.
    pub backend: Box<dyn JavaMacroBackend>,

    /// Optional LSP session pool for probe execution (jdtls / rust-analyzer).
    ///
    /// `None` at construction until Phase 4 probe bindings are wired in; the
    /// planner must check for `Some` before issuing LSP-backed probe ops.
    pub lsp: Option<LspSessionManager>,

    /// Probe runner for evaluating [`crate::model::MacroProbe`] slots
    /// at plan time.
    ///
    /// Defaults to [`UnavailableProbeRunner`] (fail-closed) until a real
    /// [`crate::probe::CodeNavProbeRunner`] is injected at service
    /// startup (Phase 4 P4b wires the planner to this runner).
    pub probe_runner: Box<dyn ProbeRunner>,
}

impl MacroPlannerContext {
    /// Construct a context with an explicit backend, optional LSP manager,
    /// and a probe runner.
    ///
    /// Pass [`UnavailableProbeRunner`] when no real runner is available yet;
    /// pass a [`crate::probe::CodeNavProbeRunner`] for production use.
    pub fn new(
        backend: Box<dyn JavaMacroBackend>,
        lsp: Option<LspSessionManager>,
        probe_runner: Box<dyn ProbeRunner>,
    ) -> Self {
        Self {
            backend,
            lsp,
            probe_runner,
        }
    }
}

impl Default for MacroPlannerContext {
    /// Returns a fail-closed context:
    /// [`UnavailableBackend`] + no LSP + [`UnavailableProbeRunner`].
    fn default() -> Self {
        Self {
            backend: Box::new(UnavailableBackend),
            lsp: None,
            probe_runner: Box::new(UnavailableProbeRunner),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{JavaEmitOp, JavaRewriteOp};

    #[test]
    fn default_context_uses_unavailable_backend() {
        let ctx = MacroPlannerContext::default();
        let op = JavaEmitOp::EmitType {
            source_root: "/repo/src/main/java".into(),
            package: "com.example".into(),
            name: "Stub".into(),
            kind: "interface".into(),
            source_text: "".into(),
        };
        let err = ctx.backend.emit(&op).unwrap_err();
        assert!(
            err.to_string().contains("error.backend_unavailable"),
            "default context should use UnavailableBackend"
        );
    }

    #[test]
    fn default_context_has_no_lsp() {
        let ctx = MacroPlannerContext::default();
        assert!(ctx.lsp.is_none(), "default context should have no LSP");
    }

    #[test]
    fn default_context_probe_runner_fails_closed() {
        use crate::expr::Context;
        use crate::model::MacroInvocation;
        use crate::probe::ProbeSpec;
        let ctx = MacroPlannerContext::default();
        let spec = ProbeSpec::CodeSymbols {
            query: None,
            item_kinds: None,
            languages: None,
            name_equals: None,
            path_contains: None,
        };
        let inv = MacroInvocation {
            macro_id: "test".into(),
            version: None,
            project_dir: "/tmp".into(),
            inputs: serde_json::Map::new(),
            anchors: None,
            operator_opt_outs: vec![],
        };
        let err = ctx
            .probe_runner
            .run_probe("probe", &spec, &Context::default(), &inv)
            .unwrap_err();
        assert!(
            err.to_string().contains("error.probe_backend_unavailable"),
            "default probe_runner should use UnavailableProbeRunner"
        );
    }

    #[test]
    fn new_constructor_accepts_unavailable_backend() {
        use crate::probe::UnavailableProbeRunner;
        let ctx = MacroPlannerContext::new(
            Box::new(UnavailableBackend),
            None,
            Box::new(UnavailableProbeRunner),
        );
        let op = JavaRewriteOp::InsertMember {
            target_file: "Foo.java".into(),
            target_type: "Foo".into(),
            member_text: "void x() {}".into(),
            imports: vec![],
        };
        let err = ctx.backend.rewrite(&op).unwrap_err();
        assert!(err.to_string().contains("error.backend_unavailable"));
    }
}
