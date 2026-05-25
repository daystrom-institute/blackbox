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
//! The backend is NOT placed on [`crate::refactor::PlanContext`] — the macro
//! layer owns its own context so it can carry Java-specific state without
//! polluting the general refactor plumbing.

use crate::lsp::LspSessionManager;
use crate::macros::backend::{JavaMacroBackend, UnavailableBackend};

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
}

impl MacroPlannerContext {
    /// Construct a context with an explicit backend and optional LSP manager.
    pub fn new(backend: Box<dyn JavaMacroBackend>, lsp: Option<LspSessionManager>) -> Self {
        Self { backend, lsp }
    }
}

impl Default for MacroPlannerContext {
    /// Returns a fail-closed context: [`UnavailableBackend`] + no LSP.
    fn default() -> Self {
        Self {
            backend: Box::new(UnavailableBackend),
            lsp: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::backend::{JavaEmitOp, JavaRewriteOp};

    #[test]
    fn default_context_uses_unavailable_backend() {
        let ctx = MacroPlannerContext::default();
        let op = JavaEmitOp::EmitType {
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
    fn new_constructor_accepts_unavailable_backend() {
        let ctx = MacroPlannerContext::new(Box::new(UnavailableBackend), None);
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
