//! Java macro backend boundary types and fail-closed stub.
//!
//! # Design
//!
//! The real Java backend (OpenRewrite + JavaPoet sidecar) is a Phase 3
//! concern. This module defines:
//!
//! - [`JavaEmitOp`] / [`JavaRewriteOp`] — typed backend operation variants.
//! - [`BackendEditSet`] — the typed output container backends produce.
//! - [`JavaMacroBackend`] — the trait the planner calls.
//! - [`UnavailableBackend`] — fail-closed default; every method returns
//!   `error.backend_unavailable`. Used as the default in
//!   [`crate::macros::MacroPlannerContext`] until the real sidecar lands.
//!
//! # Fail-closed invariant
//!
//! Per the design (`unified-code-synthesis-model.md` Java Backend section):
//! when the backend is unavailable, operations **must** fail with a typed
//! `error.backend_unavailable` error rather than silently downgrading to
//! unverified template output.

use anyhow::{Result, anyhow};

use crate::refactor::{FileCreate, FileEdit};

// ---------------------------------------------------------------------------
// BackendEditSet
// ---------------------------------------------------------------------------

/// The file-mutation output produced by a Java macro backend.
///
/// Folds into [`crate::macros::EditSet`] at planner aggregation time; never
/// written to disk directly. Matches the subset of `EditSet` that backends
/// actually produce (backends do not produce `file_moves` — those come from
/// the macro planner compositing delegate refactor ops).
#[derive(Debug, Clone, Default)]
pub struct BackendEditSet {
    /// Mutations to existing files (inline text edits).
    pub file_edits: Vec<FileEdit>,
    /// New files the backend synthesized.
    pub file_creates: Vec<FileCreate>,
}

// ---------------------------------------------------------------------------
// JavaEmitOp
// ---------------------------------------------------------------------------

/// A typed "emit new artifact" operation for the Java macro backend.
///
/// Variants cover the source-generation cases the macro layer needs. New
/// generation forms require a new variant — no raw `serde_json::Value` escape
/// hatch here so the compiler enforces completeness.
#[derive(Debug, Clone)]
pub enum JavaEmitOp {
    /// Emit a new top-level Java type (class, interface, enum, annotation,
    /// or record) as a new source file.
    EmitType {
        /// Java package, e.g. `"com.example.payments"`.
        package: String,
        /// Simple type name, e.g. `"PaymentService"`.
        name: String,
        /// Type kind: `"class"`, `"interface"`, `"enum"`, `"annotation"`, or
        /// `"record"`.
        kind: String,
        /// Full source text of the new type (including package declaration and
        /// imports). Backend validates parse-validity before returning.
        source_text: String,
    },
}

// ---------------------------------------------------------------------------
// JavaRewriteOp
// ---------------------------------------------------------------------------

/// A typed "rewrite existing source" operation for the Java macro backend.
///
/// Variants cover the source-modification cases the macro layer needs.
#[derive(Debug, Clone)]
pub enum JavaRewriteOp {
    /// Insert a new member (field, method, constructor, or nested type)
    /// into an existing type in a source file.
    InsertMember {
        /// Absolute path to the file containing the target type.
        target_file: String,
        /// Simple name of the type to insert the member into, e.g.
        /// `"PaymentServiceImpl"`.
        target_type: String,
        /// Source text of the member to insert (method signature + body,
        /// field declaration, etc.).
        member_text: String,
        /// Fully-qualified import statements to add to the file if not already
        /// present, e.g. `["com.example.core.Port"]`.
        imports: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// JavaMacroBackend trait
// ---------------------------------------------------------------------------

/// Abstraction over the Java source-generation/rewrite sidecar.
///
/// The planner calls `emit` for new-file generation and `rewrite` for
/// existing-file transformation. Both return a [`BackendEditSet`] that the
/// planner folds into the aggregate [`crate::macros::EditSet`].
///
/// Implementations must be object-safe (`&self` receivers, no generics on
/// methods) so the planner can hold a `Box<dyn JavaMacroBackend>`.
pub trait JavaMacroBackend: Send + Sync {
    /// Generate a new Java artifact via the backend (e.g. JavaPoet).
    fn emit(&self, op: &JavaEmitOp) -> Result<BackendEditSet>;

    /// Rewrite existing Java source via the backend (e.g. OpenRewrite).
    fn rewrite(&self, op: &JavaRewriteOp) -> Result<BackendEditSet>;
}

// ---------------------------------------------------------------------------
// UnavailableBackend (fail-closed stub)
// ---------------------------------------------------------------------------

/// Fail-closed default backend.
///
/// Every method returns `Err` with an `error.backend_unavailable` message.
/// Used as the [`crate::macros::MacroPlannerContext`] default until the real
/// OpenRewrite/JavaPoet sidecar is wired up in Phase 3.
///
/// This ensures the planner always fails with a clear error rather than
/// silently producing `template_only` output when the sidecar is absent.
pub struct UnavailableBackend;

impl JavaMacroBackend for UnavailableBackend {
    fn emit(&self, _op: &JavaEmitOp) -> Result<BackendEditSet> {
        Err(anyhow!(
            "error.backend_unavailable: Java macro backend is not available; \
             the OpenRewrite/JavaPoet sidecar is not connected"
        ))
    }

    fn rewrite(&self, _op: &JavaRewriteOp) -> Result<BackendEditSet> {
        Err(anyhow!(
            "error.backend_unavailable: Java macro backend is not available; \
             the OpenRewrite/JavaPoet sidecar is not connected"
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_backend_emit_returns_backend_unavailable() {
        let backend = UnavailableBackend;
        let op = JavaEmitOp::EmitType {
            package: "com.example".into(),
            name: "Foo".into(),
            kind: "interface".into(),
            source_text: "package com.example;\npublic interface Foo {}".into(),
        };
        let err = backend.emit(&op).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "emit error should contain error.backend_unavailable; got: {msg}"
        );
    }

    #[test]
    fn unavailable_backend_rewrite_returns_backend_unavailable() {
        let backend = UnavailableBackend;
        let op = JavaRewriteOp::InsertMember {
            target_file: "/repo/src/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            member_text: "public void doWork() {}".into(),
            imports: vec!["com.example.Port".into()],
        };
        let err = backend.rewrite(&op).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "rewrite error should contain error.backend_unavailable; got: {msg}"
        );
    }
}
