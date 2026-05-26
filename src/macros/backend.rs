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
use serde::{Deserialize, Serialize};

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
///
/// Serde: tagged with `op` field (e.g. `{"op": "emit_type", ...}`) so the
/// planner can decode backend_op Value leaves into a typed variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum JavaEmitOp {
    /// Emit a new top-level Java type (class, interface, enum, or record)
    /// as a new source file. (Annotation types are not supported in v1.)
    EmitType {
        /// Absolute path to the Java source root (e.g. `"src/main/java"`).
        /// Forwarded to the sidecar so it can compute the file path from the
        /// package and type name.
        source_root: String,
        /// Java package, e.g. `"com.example.payments"`.
        package: String,
        /// Simple type name, e.g. `"PaymentService"`.
        name: String,
        /// Type kind: `"class"`, `"interface"`, `"enum"`, or `"record"`.
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
///
/// Serde: tagged with `op` field (e.g. `{"op": "insert_member", ...}`) so the
/// planner can decode backend_op Value leaves into a typed variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
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

    /// Replace the body of an existing method in the target type.
    ///
    /// The method is located by `method_name` + `parameter_types` (written form,
    /// e.g. `["String", "int"]`). If no matching method is found the sidecar
    /// returns `error.method_not_found`; if more than one match exists it returns
    /// `error.method_ambiguous`.
    ReplaceMethodBody {
        /// Absolute path to the file containing the target type.
        target_file: String,
        /// Simple name of the type containing the method.
        target_type: String,
        /// Simple name of the method to replace the body of.
        method_name: String,
        /// Written parameter types used for disambiguation, e.g. `["String", "int"]`.
        /// Pass an empty list for no-arg methods.
        parameter_types: Vec<String>,
        /// New method body text (statements only, no surrounding braces).
        new_body: String,
        /// Fully-qualified import statements to add if not already present.
        imports: Vec<String>,
    },

    /// Append one or more statements to the body of an existing method.
    ///
    /// `placement` must be `"append"` (the only supported value in v1).
    InsertStatementInMethod {
        /// Absolute path to the file containing the target type.
        target_file: String,
        /// Simple name of the type containing the method.
        target_type: String,
        /// Simple name of the method to insert statements into.
        method_name: String,
        /// Written parameter types used for disambiguation.
        parameter_types: Vec<String>,
        /// Statement text to insert (one or more Java statements).
        statement_text: String,
        /// Insertion position. Must be `"append"` in v1.
        placement: String,
        /// Fully-qualified import statements to add if not already present.
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

    /// Rewrite with an optional source content override.
    ///
    /// When `source_override = Some((content, original_sha256, original_byte_len))` the
    /// backend MUST use `content` as the source text sent to the sidecar (instead of
    /// reading the file from disk), MUST preserve `original_sha256` in the returned
    /// `FileEdit`, and MUST use `original_byte_len` as the `byte_end` of the
    /// full-span `TextEdit` (not `content.len()`).  Together these ensure the
    /// `FileEdit` is anchored to the on-disk original: `original_sha256` lets
    /// `refactor::apply` verify the file is unchanged, and `byte_end = original_byte_len`
    /// produces a valid range against the on-disk file length.
    ///
    /// This allows the planner to chain multiple Rewrite operations on the same
    /// file without writing intermediate results to disk: op N gets op N-1's
    /// output as its source, while the final `FileEdit` still carries the
    /// SHA-256 and byte length of the unmodified on-disk file so `refactor::apply`
    /// can verify the file hasn't changed between plan and apply.
    ///
    /// Default: ignores `source_override` and delegates to [`Self::rewrite`].
    /// Backends that only ever process one Rewrite per file can leave the
    /// default; backends that must support sequential same-file rewrites
    /// (e.g. [`super::sidecar_backend::SidecarBackend`]) must override.
    fn rewrite_with_source_override(
        &self,
        op: &JavaRewriteOp,
        _source_override: Option<(&str, &str, usize)>,
    ) -> Result<BackendEditSet> {
        self.rewrite(op)
    }
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
            source_root: "/repo/src/main/java".into(),
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

    #[test]
    fn java_emit_op_serde_round_trip() {
        use serde_json::json;
        let op = JavaEmitOp::EmitType {
            source_root: "/repo/src/main/java".into(),
            package: "com.example".into(),
            name: "PaymentService".into(),
            kind: "interface".into(),
            source_text: "package com.example;\npublic interface PaymentService {}".into(),
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["op"], json!("emit_type"), "tagged op field");
        assert_eq!(v["source_root"], json!("/repo/src/main/java"));
        assert_eq!(v["package"], json!("com.example"));
        let back: JavaEmitOp = serde_json::from_value(v).unwrap();
        match back {
            JavaEmitOp::EmitType { name, source_root, .. } => {
                assert_eq!(name, "PaymentService");
                assert_eq!(source_root, "/repo/src/main/java");
            }
        }
    }

    #[test]
    fn java_rewrite_op_serde_round_trip() {
        use serde_json::json;
        let op = JavaRewriteOp::InsertMember {
            target_file: "/repo/src/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            member_text: "public void doWork() {}".into(),
            imports: vec!["com.example.Port".into()],
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["op"], json!("insert_member"), "tagged op field");
        assert_eq!(v["target_file"], json!("/repo/src/FooImpl.java"));
        let back: JavaRewriteOp = serde_json::from_value(v).unwrap();
        match back {
            JavaRewriteOp::InsertMember { target_type, imports, .. } => {
                assert_eq!(target_type, "FooImpl");
                assert_eq!(imports, vec!["com.example.Port"]);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn java_rewrite_op_replace_method_body_serde_round_trip() {
        use serde_json::json;
        let op = JavaRewriteOp::ReplaceMethodBody {
            target_file: "/repo/src/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            method_name: "doWork".into(),
            parameter_types: vec!["String".into(), "int".into()],
            new_body: "return input.length() + n;".into(),
            imports: vec![],
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["op"], json!("replace_method_body"));
        assert_eq!(v["method_name"], json!("doWork"));
        assert_eq!(v["parameter_types"][0], json!("String"));
        let back: JavaRewriteOp = serde_json::from_value(v).unwrap();
        match back {
            JavaRewriteOp::ReplaceMethodBody { method_name, parameter_types, new_body, .. } => {
                assert_eq!(method_name, "doWork");
                assert_eq!(parameter_types, vec!["String", "int"]);
                assert!(new_body.contains("length"));
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn java_rewrite_op_insert_statement_in_method_serde_round_trip() {
        use serde_json::json;
        let op = JavaRewriteOp::InsertStatementInMethod {
            target_file: "/repo/src/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            method_name: "init".into(),
            parameter_types: vec![],
            statement_text: "this.ready = true;".into(),
            placement: "append".into(),
            imports: vec![],
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["op"], json!("insert_statement_in_method"));
        assert_eq!(v["placement"], json!("append"));
        let back: JavaRewriteOp = serde_json::from_value(v).unwrap();
        match back {
            JavaRewriteOp::InsertStatementInMethod {
                method_name,
                placement,
                statement_text,
                ..
            } => {
                assert_eq!(method_name, "init");
                assert_eq!(placement, "append");
                assert!(statement_text.contains("ready"));
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn java_emit_op_decode_fails_on_unknown_op() {
        use serde_json::json;
        let v = json!({"op": "unknown_emit_op", "package": "com.x", "name": "X"});
        let err = serde_json::from_value::<JavaEmitOp>(v);
        assert!(err.is_err(), "unknown op tag should fail to decode");
    }
}
