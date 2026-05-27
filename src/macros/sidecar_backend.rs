//! [`SidecarBackend`] — [`JavaMacroBackend`] implementation backed by the
//! Java worker sidecar (`JavaWorkerPool`).
//!
//! # Fail-closed invariant
//!
//! When the sidecar is unavailable (JAR absent, JVM missing, worker spawn
//! failed, or the RPC call itself errors), every method returns
//! `error.backend_unavailable`. The caller never silently downgrades to
//! unverified template output (mirrors RX-V3).
//!
//! # emit (JavaEmitOp::EmitType)
//!
//! Calls `emitType` on the Java worker. Maps `result.file_creates` to
//! `BackendEditSet.file_creates`. RPC error → `error.backend_unavailable`.
//!
//! # rewrite (JavaRewriteOp::*)
//!
//! All three variants read `target_file` from disk, compute `original_sha256`,
//! call the corresponding JSON-RPC method, and convert the result:
//!
//! - `InsertMember` → `insertMember` RPC. Maps `error.member_conflict` through.
//! - `ReplaceMethodBody` → `replaceMethodBody` RPC. Maps `error.method_not_found`
//!   and `error.method_ambiguous` through as domain errors.
//! - `InsertStatementInMethod` → `insertStatementInMethod` RPC. Same error mapping.
//! - `InsertClassAnnotation` → `insertClassAnnotation` RPC. Adds a class-level
//!   annotation (idempotent: no-op when an annotation of the same simple name
//!   is already present).
//! - `DeleteMember` → `deleteMember` RPC. Removes a member by name (+ optional
//!   parameter types); no-op when absent, `error.member_ambiguous` on an
//!   ambiguous name-only match.
//! - `InsertFieldAnnotation` → `insertFieldAnnotation` RPC. Adds an annotation
//!   above a named field (idempotent; per-field analogue of the class form).
//! - `PruneUnusedImport` → `pruneUnusedImport` RPC. Removes named imports only
//!   when unreferenced (still-referenced imports are left; absent → no-op).
//!
//! `no_op=true` → empty `BackendEditSet`.
//! `changed=true` → single full-span `FileEdit` replacing the whole file.
//! `changed=false && no_op=false` → `error.backend_unavailable` (protocol violation).

#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use super::backend::{BackendEditSet, JavaEmitOp, JavaMacroBackend, JavaRewriteOp};
use super::java_sidecar::JavaWorkerPool;
use super::java_sidecar_protocol::{
    DeleteMemberParams, EmitTypeParams, InsertClassAnnotationParams, InsertFieldAnnotationParams,
    InsertMemberParams, InsertStatementInMethodParams, PruneUnusedImportParams,
    ReplaceMethodBodyParams, METHOD_DELETE_MEMBER, METHOD_EMIT_TYPE, METHOD_INSERT_CLASS_ANNOTATION,
    METHOD_INSERT_FIELD_ANNOTATION, METHOD_INSERT_MEMBER, METHOD_INSERT_STATEMENT_IN_METHOD,
    METHOD_PRUNE_UNUSED_IMPORT, METHOD_REPLACE_METHOD_BODY,
};
use crate::refactor::{FileCreate, FileEdit, TextEdit};

// ── SidecarBackend ────────────────────────────────────────────────────────────

/// [`JavaMacroBackend`] implementation that delegates to the Java worker sidecar.
///
/// Holds a reference to the process-wide `JavaWorkerPool` and the project root
/// so the pool can locate or spawn the per-project worker on first use.
pub struct SidecarBackend {
    pool: &'static JavaWorkerPool,
    project_root: PathBuf,
}

impl SidecarBackend {
    /// Construct a `SidecarBackend` for the given project root.
    ///
    /// Uses the process-wide `JavaWorkerPool` singleton. If the JAR env var is
    /// absent, operations will fail closed at call time.
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            pool: super::java_sidecar::pool(),
            project_root,
        }
    }
}

impl JavaMacroBackend for SidecarBackend {
    fn emit(&self, op: &JavaEmitOp) -> Result<BackendEditSet> {
        let JavaEmitOp::EmitType {
            source_root,
            package,
            name,
            kind,
            source_text,
        } = op;

        let worker_arc = self.pool.worker_for(&self.project_root).map_err(|e| {
            anyhow!(
                "error.backend_unavailable: Java macro sidecar unavailable for emit: {e}"
            )
        })?;

        let result: super::java_sidecar_protocol::EmitTypeResult = worker_arc
            .lock()
            .unwrap()
            .call(
                METHOD_EMIT_TYPE,
                EmitTypeParams {
                    source_root: source_root.clone(),
                    package: package.clone(),
                    name: name.clone(),
                    kind: kind.clone(),
                    source_text: source_text.clone(),
                },
            )
            .map_err(|e| {
                anyhow!(
                    "error.backend_unavailable: emitType RPC failed: {e}"
                )
            })?;

        let file_creates: Vec<FileCreate> = result
            .file_creates
            .into_iter()
            .map(|fc| FileCreate {
                path: fc.path,
                content: fc.content,
            })
            .collect();

        // R2: empty file_creates is a protocol violation — fail closed rather
        // than returning a vacuous Ok that the planner would silently accept.
        if file_creates.is_empty() {
            return Err(anyhow!(
                "error.backend_unavailable: emitType returned no file_creates; \
                 the worker produced a success response but did not emit any files"
            ));
        }

        // Path containment: every worker-returned path must be within project_root.
        // The files don't exist yet so we use lexical normalization — reject any
        // path containing `..` components and require the path starts with the
        // canonicalized project root. This prevents the worker from causing the
        // planner to surface content from outside the project boundary.
        let canonical_root = self.canonical_root()?;
        for fc in &file_creates {
            check_emit_path_contained(&fc.path, &canonical_root)?;
        }

        Ok(BackendEditSet {
            file_edits: vec![],
            file_creates,
        })
    }

    fn rewrite(&self, op: &JavaRewriteOp) -> Result<BackendEditSet> {
        // Compute canonical project root once; all rewrite arms use it for
        // containment checking before reading the target file from disk.
        let canonical_root = self.canonical_root()?;

        match op {
            JavaRewriteOp::InsertMember {
                target_file,
                target_type,
                member_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertMember")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::InsertMemberResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_MEMBER,
                        InsertMemberParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            target_type: target_type.clone(),
                            member_text: member_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertMember"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertMember",
                )
            }

            JavaRewriteOp::ReplaceMethodBody {
                target_file,
                target_type,
                method_name,
                parameter_types,
                new_body,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/replaceMethodBody")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::ReplaceMethodBodyResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_REPLACE_METHOD_BODY,
                        ReplaceMethodBodyParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            target_type: target_type.clone(),
                            method_name: method_name.clone(),
                            parameter_types: parameter_types.clone(),
                            new_body: new_body.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "replaceMethodBody"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "replaceMethodBody",
                )
            }

            JavaRewriteOp::InsertStatementInMethod {
                target_file,
                target_type,
                method_name,
                parameter_types,
                statement_text,
                placement,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertStatementInMethod")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::InsertStatementInMethodResult =
                    worker_arc
                        .lock()
                        .unwrap()
                        .call(
                            METHOD_INSERT_STATEMENT_IN_METHOD,
                            InsertStatementInMethodParams {
                                target_file: target_file.clone(),
                                source_text: source_text.clone(),
                                target_type: target_type.clone(),
                                method_name: method_name.clone(),
                                parameter_types: parameter_types.clone(),
                                statement_text: statement_text.clone(),
                                placement: placement.clone(),
                                imports: imports.clone(),
                            },
                        )
                        .map_err(|e| classify_rewrite_rpc_err(e, "insertStatementInMethod"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertStatementInMethod",
                )
            }

            JavaRewriteOp::InsertClassAnnotation {
                target_file,
                target_type,
                annotation_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertClassAnnotation")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::InsertClassAnnotationResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_CLASS_ANNOTATION,
                        InsertClassAnnotationParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            target_type: target_type.clone(),
                            annotation_text: annotation_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertClassAnnotation"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertClassAnnotation",
                )
            }

            JavaRewriteOp::DeleteMember {
                target_file,
                target_type,
                member_name,
                parameter_types,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/deleteMember")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::DeleteMemberResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_DELETE_MEMBER,
                        DeleteMemberParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            target_type: target_type.clone(),
                            member_name: member_name.clone(),
                            parameter_types: parameter_types.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "deleteMember"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "deleteMember",
                )
            }

            JavaRewriteOp::InsertFieldAnnotation {
                target_file,
                target_type,
                field_name,
                annotation_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertFieldAnnotation")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::InsertFieldAnnotationResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_FIELD_ANNOTATION,
                        InsertFieldAnnotationParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            target_type: target_type.clone(),
                            field_name: field_name.clone(),
                            annotation_text: annotation_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertFieldAnnotation"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertFieldAnnotation",
                )
            }

            JavaRewriteOp::PruneUnusedImport {
                target_file,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/pruneUnusedImport")?;
                let (source_text, original_sha256) = read_source_and_sha(target_file)?;
                let result: super::java_sidecar_protocol::PruneUnusedImportResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_PRUNE_UNUSED_IMPORT,
                        PruneUnusedImportParams {
                            target_file: target_file.clone(),
                            source_text: source_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "pruneUnusedImport"))?;
                build_rewrite_edit_set(
                    target_file,
                    source_text.len(),
                    original_sha256,
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "pruneUnusedImport",
                )
            }
        }
    }

    fn rewrite_with_source_override(
        &self,
        op: &JavaRewriteOp,
        source_override: Option<(&str, &str, usize)>,
    ) -> anyhow::Result<BackendEditSet> {
        // When no override is supplied, fall back to the standard disk-read path.
        let Some((override_content, original_sha256_on_disk, original_byte_len)) = source_override
        else {
            return self.rewrite(op);
        };

        let canonical_root = self.canonical_root()?;

        // For each rewrite variant, use override_content as source_text and
        // original_sha256_on_disk as the FileEdit's original_sha256.  This keeps
        // the plan anchored to the on-disk hash so refactor::apply can verify
        // the file hasn't changed between plan and apply time.
        match op {
            JavaRewriteOp::InsertMember {
                target_file,
                target_type,
                member_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertMember(override)")?;
                let result: super::java_sidecar_protocol::InsertMemberResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_MEMBER,
                        InsertMemberParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            target_type: target_type.clone(),
                            member_text: member_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertMember"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertMember(override)",
                )
            }

            JavaRewriteOp::ReplaceMethodBody {
                target_file,
                target_type,
                method_name,
                parameter_types,
                new_body,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/replaceMethodBody(override)")?;
                let result: super::java_sidecar_protocol::ReplaceMethodBodyResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_REPLACE_METHOD_BODY,
                        ReplaceMethodBodyParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            target_type: target_type.clone(),
                            method_name: method_name.clone(),
                            parameter_types: parameter_types.clone(),
                            new_body: new_body.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "replaceMethodBody"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "replaceMethodBody(override)",
                )
            }

            JavaRewriteOp::InsertStatementInMethod {
                target_file,
                target_type,
                method_name,
                parameter_types,
                statement_text,
                placement,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertStatementInMethod(override)")?;
                let result: super::java_sidecar_protocol::InsertStatementInMethodResult =
                    worker_arc
                        .lock()
                        .unwrap()
                        .call(
                            METHOD_INSERT_STATEMENT_IN_METHOD,
                            InsertStatementInMethodParams {
                                target_file: target_file.clone(),
                                source_text: override_content.to_string(),
                                target_type: target_type.clone(),
                                method_name: method_name.clone(),
                                parameter_types: parameter_types.clone(),
                                statement_text: statement_text.clone(),
                                placement: placement.clone(),
                                imports: imports.clone(),
                            },
                        )
                        .map_err(|e| classify_rewrite_rpc_err(e, "insertStatementInMethod"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertStatementInMethod(override)",
                )
            }

            JavaRewriteOp::InsertClassAnnotation {
                target_file,
                target_type,
                annotation_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertClassAnnotation(override)")?;
                let result: super::java_sidecar_protocol::InsertClassAnnotationResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_CLASS_ANNOTATION,
                        InsertClassAnnotationParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            target_type: target_type.clone(),
                            annotation_text: annotation_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertClassAnnotation"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertClassAnnotation(override)",
                )
            }

            JavaRewriteOp::DeleteMember {
                target_file,
                target_type,
                member_name,
                parameter_types,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/deleteMember(override)")?;
                let result: super::java_sidecar_protocol::DeleteMemberResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_DELETE_MEMBER,
                        DeleteMemberParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            target_type: target_type.clone(),
                            member_name: member_name.clone(),
                            parameter_types: parameter_types.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "deleteMember"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "deleteMember(override)",
                )
            }

            JavaRewriteOp::InsertFieldAnnotation {
                target_file,
                target_type,
                field_name,
                annotation_text,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/insertFieldAnnotation(override)")?;
                let result: super::java_sidecar_protocol::InsertFieldAnnotationResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_INSERT_FIELD_ANNOTATION,
                        InsertFieldAnnotationParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            target_type: target_type.clone(),
                            field_name: field_name.clone(),
                            annotation_text: annotation_text.clone(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "insertFieldAnnotation"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "insertFieldAnnotation(override)",
                )
            }

            JavaRewriteOp::PruneUnusedImport {
                target_file,
                imports,
            } => {
                check_rewrite_path_contained(target_file, &canonical_root)?;
                let worker_arc = self.acquire_worker("rewrite/pruneUnusedImport(override)")?;
                let result: super::java_sidecar_protocol::PruneUnusedImportResult = worker_arc
                    .lock()
                    .unwrap()
                    .call(
                        METHOD_PRUNE_UNUSED_IMPORT,
                        PruneUnusedImportParams {
                            target_file: target_file.clone(),
                            source_text: override_content.to_string(),
                            imports: imports.clone(),
                        },
                    )
                    .map_err(|e| classify_rewrite_rpc_err(e, "pruneUnusedImport"))?;
                build_rewrite_edit_set(
                    target_file,
                    original_byte_len,
                    original_sha256_on_disk.to_string(),
                    result.rewritten_source,
                    result.changed,
                    result.no_op,
                    "pruneUnusedImport(override)",
                )
            }
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

impl SidecarBackend {
    /// Return the canonicalized project root, failing closed if the path cannot
    /// be resolved (e.g. the project directory was removed since startup).
    fn canonical_root(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.project_root).map_err(|e| {
            anyhow!(
                "error.backend_unavailable: cannot canonicalize project root '{}': {e}",
                self.project_root.display()
            )
        })
    }

    /// Acquire a worker from the pool, wrapping pool errors as `error.backend_unavailable`.
    fn acquire_worker(
        &self,
        op_name: &str,
    ) -> Result<std::sync::Arc<std::sync::Mutex<super::java_sidecar::JavaWorker>>> {
        self.pool
            .worker_for(&self.project_root)
            .map_err(|e| anyhow!("error.backend_unavailable: Java macro sidecar unavailable for {op_name}: {e}"))
    }
}

/// Read a source file from disk and compute its SHA-256 preimage hash.
fn read_source_and_sha(path: &str) -> Result<(String, String)> {
    let source_text = std::fs::read_to_string(path).map_err(|e| {
        anyhow!(
            "error.backend_unavailable: cannot read target file '{}': {e}",
            path
        )
    })?;
    let sha = {
        let mut hasher = Sha256::new();
        hasher.update(source_text.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    Ok((source_text, sha))
}

/// Classify a rewrite RPC error as a domain passthrough or `error.backend_unavailable`.
///
/// The sidecar surfaces domain errors (e.g. `error.member_conflict`) as JSON-RPC
/// errors whose message text contains the error code. We propagate those as-is so
/// the macro planner can surface them to the operator instead of masking them as
/// `error.backend_unavailable`.
fn classify_rewrite_rpc_err(e: anyhow::Error, rpc_method: &str) -> anyhow::Error {
    let msg = e.to_string();
    // Domain error codes to propagate verbatim from the worker's RPC error message.
    for domain_code in &[
        "error.member_conflict",
        "error.member_ambiguous",
        "error.method_not_found",
        "error.method_ambiguous",
        "error.parse_invalid",
        "error.type_mismatch",
    ] {
        if msg.contains(domain_code) {
            return anyhow!("{domain_code}: {e}");
        }
    }
    anyhow!("error.backend_unavailable: {rpc_method} RPC failed: {e}")
}

/// Convert a rewrite result (changed/no_op flags + rewritten source) to a `BackendEditSet`.
///
/// - `no_op=true` → empty set (idempotent, no changes needed).
/// - `changed=true` → single full-span `FileEdit` replacing the entire original file.
/// - Both false → protocol violation (R2 invariant); returns `error.backend_unavailable`.
///
/// `original_byte_len` is the byte length of the **on-disk** original file (not the
/// override/intermediate content).  The `TextEdit` is always `[0, original_byte_len)` so
/// that `refactor::apply` can validate the range against the on-disk file before applying.
/// For the non-override path, pass `source_text.len()`.  For the override path, pass the
/// length from the pending chain (established by the first disk-read rewrite for this file).
fn build_rewrite_edit_set(
    path: &str,
    original_byte_len: usize,
    original_sha256: String,
    rewritten_source: String,
    changed: bool,
    no_op: bool,
    rpc_method: &str,
) -> Result<BackendEditSet> {
    if no_op {
        return Ok(BackendEditSet::default());
    }
    if changed {
        let byte_end = original_byte_len;
        let edit = FileEdit {
            path: path.to_string(),
            original_sha256,
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end,
                replacement: rewritten_source.clone(),
            }],
            new_text: Some(rewritten_source),
        };
        return Ok(BackendEditSet {
            file_edits: vec![edit],
            file_creates: vec![],
        });
    }
    // R2: changed=false AND no_op=false is an invalid worker response.
    Err(anyhow!(
        "error.backend_unavailable: {rpc_method} returned changed=false no_op=false; \
         the worker response is ambiguous (expected exactly one flag set)"
    ))
}

/// Check that a rewrite target file is within the canonical project root.
///
/// Uses `std::fs::canonicalize` (resolves symlinks) on the target path.
/// The target file must already exist on disk (rewrite pre-condition); if it
/// cannot be canonicalized the path is missing or inaccessible — both are
/// rejected with `error.path_escape`.
fn check_rewrite_path_contained(path: &str, canonical_root: &std::path::Path) -> Result<()> {
    let canonical_target = std::fs::canonicalize(path).map_err(|e| {
        anyhow!(
            "error.path_escape: rewrite target '{}' could not be resolved: {e}",
            path
        )
    })?;
    if !canonical_target.starts_with(canonical_root) {
        return Err(anyhow!(
            "error.path_escape: rewrite target '{}' is outside the macro project root",
            path
        ));
    }
    Ok(())
}

/// Check that an emit target path is within the canonical project root.
///
/// The file does not exist yet (emit creates it), so `std::fs::canonicalize`
/// cannot be called on the full path. Instead we use lexical normalization:
/// reject any `..` component anywhere in the path, then verify the path starts
/// with the canonical project root. This defeats `..`-based traversal while
/// still allowing any valid in-project path.
fn check_emit_path_contained(path: &str, canonical_root: &std::path::Path) -> Result<()> {
    let p = PathBuf::from(path);
    // Reject any `..` component — defeats path traversal without canonicalize.
    for component in p.components() {
        if component == std::path::Component::ParentDir {
            return Err(anyhow!(
                "error.path_escape: emit target '{}' contains '..' and is outside \
                 the macro project root",
                path
            ));
        }
    }
    if !p.starts_with(canonical_root) {
        return Err(anyhow!(
            "error.path_escape: emit target '{}' is outside the macro project root",
            path
        ));
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend() -> SidecarBackend {
        SidecarBackend::new(PathBuf::from("/tmp/test-project"))
    }

    // ── Path containment tests ────────────────────────────────────────────────

    #[test]
    fn rewrite_containment_rejects_absolute_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
        // /etc/passwd exists on Linux and is unambiguously outside any tempdir.
        let result = check_rewrite_path_contained("/etc/passwd", &canonical_root);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("error.path_escape"),
            "absolute outside path should yield error.path_escape; got: {err}"
        );
    }

    #[test]
    fn emit_containment_rejects_dotdot_component() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
        // Build a path that tries to escape via `..`.
        let escape = format!("{}/../escape.java", canonical_root.display());
        let result = check_emit_path_contained(&escape, &canonical_root);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("error.path_escape"),
            "dotdot in emit path should yield error.path_escape; got: {err}"
        );
    }

    #[test]
    fn emit_containment_rejects_absolute_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
        let result = check_emit_path_contained("/etc/shadow", &canonical_root);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("error.path_escape"),
            "absolute outside path should yield error.path_escape; got: {err}"
        );
    }

    #[test]
    fn containment_passes_for_in_project_paths() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();

        // Emit: file doesn't exist yet — lexical check only.
        let emit_target = canonical_root.join("src/main/java/com/example/Foo.java");
        assert!(
            check_emit_path_contained(emit_target.to_str().unwrap(), &canonical_root).is_ok(),
            "in-project emit path should pass containment"
        );

        // Rewrite: file must exist on disk for canonicalize to work.
        let rewrite_target = dir.path().join("FooImpl.java");
        std::fs::write(&rewrite_target, "// placeholder").unwrap();
        assert!(
            check_rewrite_path_contained(
                rewrite_target.to_str().unwrap(),
                &canonical_root
            )
            .is_ok(),
            "in-project rewrite path should pass containment"
        );
    }

    #[test]
    fn emit_fails_closed_when_jar_unset() {
        if jar_present() {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                 skipping unavailability test"
            );
            return;
        }
        let backend = make_backend();
        let op = JavaEmitOp::EmitType {
            source_root: "/repo/src/main/java".into(),
            package: "com.example".into(),
            name: "Stub".into(),
            kind: "interface".into(),
            source_text: "package com.example;\npublic interface Stub {}".into(),
        };
        let err = backend.emit(&op).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "emit should yield error.backend_unavailable when JAR absent; got: {msg}"
        );
    }

    fn jar_present() -> bool {
        std::env::var("BLACKBOX_JAVA_WORKER_JAR")
            .ok()
            .filter(|j| !j.trim().is_empty())
            .map(|j| PathBuf::from(&j).exists())
            .unwrap_or(false)
    }

    #[test]
    fn rewrite_fails_closed_when_jar_unset() {
        if jar_present() {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                 skipping unavailability test"
            );
            return;
        }
        let backend = make_backend();
        let op = JavaRewriteOp::InsertMember {
            target_file: "/tmp/nonexistent/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            member_text: "public void doWork() {}".into(),
            imports: vec![],
        };
        let err = backend.rewrite(&op).unwrap_err();
        let msg = err.to_string();
        // Either the pool fails (backend_unavailable) or the file-read fails.
        // Both are acceptable fail-closed behaviours.
        assert!(
            msg.contains("error.backend_unavailable") || msg.contains("cannot read target file"),
            "rewrite should fail closed when JAR absent; got: {msg}"
        );
    }

    #[test]
    fn replace_method_body_fails_closed_when_jar_unset() {
        if jar_present() {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                 skipping unavailability test"
            );
            return;
        }
        let backend = make_backend();
        let op = JavaRewriteOp::ReplaceMethodBody {
            target_file: "/tmp/nonexistent/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            method_name: "doWork".into(),
            parameter_types: vec![],
            new_body: "return 42;".into(),
            imports: vec![],
        };
        let err = backend.rewrite(&op).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable") || msg.contains("cannot read target file"),
            "replaceMethodBody should fail closed when JAR absent; got: {msg}"
        );
    }

    #[test]
    fn insert_statement_in_method_fails_closed_when_jar_unset() {
        if jar_present() {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                 skipping unavailability test"
            );
            return;
        }
        let backend = make_backend();
        let op = JavaRewriteOp::InsertStatementInMethod {
            target_file: "/tmp/nonexistent/FooImpl.java".into(),
            target_type: "FooImpl".into(),
            method_name: "init".into(),
            parameter_types: vec![],
            statement_text: "this.ready = true;".into(),
            placement: "append".into(),
            imports: vec![],
        };
        let err = backend.rewrite(&op).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable") || msg.contains("cannot read target file"),
            "insertStatementInMethod should fail closed when JAR absent; got: {msg}"
        );
    }

    #[test]
    fn live_emit_test_skips_when_jar_absent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — skipping live emit test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!("[sidecar_backend] worker JAR missing — skipping live test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let backend = SidecarBackend::new(dir.path().to_path_buf());
        let op = JavaEmitOp::EmitType {
            source_root: dir.path().to_str().unwrap().to_string(),
            package: "com.example".into(),
            name: "LiveTest".into(),
            kind: "interface".into(),
            source_text: "package com.example;\npublic interface LiveTest {}\n".into(),
        };
        match backend.emit(&op) {
            Ok(bes) => {
                assert!(
                    !bes.file_creates.is_empty(),
                    "live emit should produce at least one file_create"
                );
                let paths: Vec<&str> =
                    bes.file_creates.iter().map(|fc| fc.path.as_str()).collect();
                assert!(
                    paths.iter().any(|p| p.contains("LiveTest")),
                    "emitted path should contain type name 'LiveTest'; got: {paths:?}"
                );
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("error.backend_unavailable"),
                    "live emit error must be error.backend_unavailable; got: {msg}"
                );
            }
        }
    }

    #[test]
    fn live_replace_method_body_test_skips_when_jar_absent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live replaceMethodBody test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!(
                "[sidecar_backend] worker JAR missing — skipping live replaceMethodBody test"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("LiveBodyTarget.java");
        std::fs::write(
            &target_file,
            "package com.example;\npublic class LiveBodyTarget {\n    \
             public void greet() {\n        System.out.println(\"old\");\n    }\n}\n",
        )
        .expect("failed to write temp .java file");

        let backend = SidecarBackend::new(dir.path().to_path_buf());
        let op = JavaRewriteOp::ReplaceMethodBody {
            target_file: target_file.to_str().unwrap().to_string(),
            target_type: "LiveBodyTarget".into(),
            method_name: "greet".into(),
            parameter_types: vec![],
            new_body: "System.out.println(\"new\");".into(),
            imports: vec![],
        };
        match backend.rewrite(&op) {
            Ok(bes) => {
                if !bes.file_edits.is_empty() {
                    let rewritten = bes.file_edits[0].new_text.as_deref().unwrap_or("");
                    assert!(
                        rewritten.contains("new"),
                        "rewritten source should contain 'new'; got:\n{rewritten}"
                    );
                    assert!(
                        !rewritten.contains("\"old\""),
                        "old body should be replaced; got:\n{rewritten}"
                    );
                }
                // no_op (empty edits) is also acceptable.
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("error.backend_unavailable")
                        || msg.contains("error.method_not_found"),
                    "live replaceMethodBody error must be backend_unavailable or method_not_found; \
                     got: {msg}"
                );
            }
        }
    }

    #[test]
    fn live_insert_statement_in_method_test_skips_when_jar_absent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live insertStatementInMethod test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!(
                "[sidecar_backend] worker JAR missing — skipping live insertStatementInMethod test"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("LiveStmtTarget.java");
        std::fs::write(
            &target_file,
            "package com.example;\npublic class LiveStmtTarget {\n    \
             public void init() {\n    }\n}\n",
        )
        .expect("failed to write temp .java file");

        let backend = SidecarBackend::new(dir.path().to_path_buf());
        let op = JavaRewriteOp::InsertStatementInMethod {
            target_file: target_file.to_str().unwrap().to_string(),
            target_type: "LiveStmtTarget".into(),
            method_name: "init".into(),
            parameter_types: vec![],
            statement_text: "this.ready = true;".into(),
            placement: "append".into(),
            imports: vec![],
        };
        match backend.rewrite(&op) {
            Ok(bes) => {
                if !bes.file_edits.is_empty() {
                    let rewritten = bes.file_edits[0].new_text.as_deref().unwrap_or("");
                    assert!(
                        rewritten.contains("ready"),
                        "rewritten source should contain 'ready'; got:\n{rewritten}"
                    );
                }
                // no_op is also acceptable if statement already existed.
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("error.backend_unavailable")
                        || msg.contains("error.method_not_found"),
                    "live insertStatementInMethod error must be backend_unavailable or \
                     method_not_found; got: {msg}"
                );
            }
        }
    }

    #[test]
    fn live_rewrite_test_skips_when_jar_absent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — skipping live rewrite test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!("[sidecar_backend] worker JAR missing — skipping live rewrite test");
            return;
        }

        // Write a temp .java file on disk — rewrite reads it.
        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("LiveTarget.java");
        std::fs::write(
            &target_file,
            "package com.example;\npublic class LiveTarget {\n}\n",
        )
        .expect("failed to write temp .java file");

        let backend = SidecarBackend::new(dir.path().to_path_buf());
        let op = JavaRewriteOp::InsertMember {
            target_file: target_file.to_str().unwrap().to_string(),
            target_type: "LiveTarget".into(),
            member_text: "public void liveMethod() {}".into(),
            imports: vec![],
        };
        match backend.rewrite(&op) {
            Ok(bes) => {
                if !bes.file_edits.is_empty() {
                    // Changed: rewritten source must contain the inserted method.
                    let rewritten = bes.file_edits[0].new_text.as_deref().unwrap_or("");
                    assert!(
                        rewritten.contains("liveMethod"),
                        "rewritten source should contain 'liveMethod'; got:\n{rewritten}"
                    );
                }
                // no_op (empty edits) is also acceptable if the method already existed.
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("error.backend_unavailable")
                        || msg.contains("error.member_conflict"),
                    "live rewrite error must be backend_unavailable or member_conflict; got: {msg}"
                );
            }
        }
    }

    /// End-to-end proof for `InsertClassAnnotation` (Phase 6, gap 1).
    ///
    /// Unlike the lenient `live_*` tests above, the happy path here is
    /// **strict**: with a real worker the op MUST change the file, the rewritten
    /// source MUST carry the annotation and its import, and a second run against
    /// the already-annotated source MUST be a no-op. This is what actually
    /// exercises the OpenRewrite `JavaTemplate.addAnnotation` idiom — the part
    /// that compiling alone does not prove.
    #[test]
    fn live_insert_class_annotation_adds_then_is_idempotent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live insertClassAnnotation test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!(
                "[sidecar_backend] worker JAR missing — skipping live insertClassAnnotation test"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("User.java");
        std::fs::write(
            &target_file,
            "package com.example;\n\npublic class User {\n    private String name;\n}\n",
        )
        .expect("failed to write temp .java file");
        let target_str = target_file.to_str().unwrap().to_string();

        let backend = SidecarBackend::new(dir.path().to_path_buf());
        let op = JavaRewriteOp::InsertClassAnnotation {
            target_file: target_str.clone(),
            target_type: "User".into(),
            annotation_text: "@Getter".into(),
            imports: vec!["lombok.Getter".into()],
        };

        // -- (1) Happy path: STRICT — must change and produce a valid result ----
        let bes = backend
            .rewrite(&op)
            .expect("live insertClassAnnotation must succeed with a real worker");
        assert_eq!(
            bes.file_edits.len(),
            1,
            "insertClassAnnotation must produce exactly one file edit on a fresh class"
        );
        let rewritten = bes.file_edits[0]
            .new_text
            .clone()
            .expect("changed FileEdit must carry new_text");
        assert!(
            rewritten.contains("@Getter"),
            "rewritten source must contain the @Getter annotation; got:\n{rewritten}"
        );
        assert!(
            rewritten.contains("import lombok.Getter;"),
            "rewritten source must contain the lombok.Getter import; got:\n{rewritten}"
        );
        // The annotation must precede the class keyword (class-level placement).
        let anno_at = rewritten.find("@Getter").unwrap();
        let class_at = rewritten.find("class User").unwrap();
        assert!(
            anno_at < class_at,
            "@Getter must be placed above the class declaration; got:\n{rewritten}"
        );
        // The original member must survive untouched.
        assert!(
            rewritten.contains("private String name;"),
            "the original field must be preserved; got:\n{rewritten}"
        );

        // -- (2) Idempotency: STRICT — re-run on annotated source is a no-op ----
        std::fs::write(&target_file, &rewritten).expect("write rewritten source back to disk");
        let bes2 = backend
            .rewrite(&op)
            .expect("second insertClassAnnotation run must succeed");
        assert!(
            bes2.file_edits.is_empty(),
            "second run on already-annotated source must be a no-op (empty edit set); \
             got {} edit(s)",
            bes2.file_edits.len()
        );
    }

    /// End-to-end proof for `DeleteMember` (Phase 6, gap 2). Strict: deletes a
    /// named method, leaves siblings intact, is idempotent on re-run, and fails
    /// closed (`error.member_ambiguous`) on an ambiguous name-only match.
    #[test]
    fn live_delete_member_removes_then_idempotent_and_ambiguity_fails() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live deleteMember test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!("[sidecar_backend] worker JAR missing — skipping live deleteMember test");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("Bean.java");
        std::fs::write(
            &target_file,
            "package com.example;\n\npublic class Bean {\n    \
             private String name;\n    \
             public String getName() { return name; }\n    \
             public void setName(String name) { this.name = name; }\n}\n",
        )
        .expect("write temp .java file");
        let target_str = target_file.to_str().unwrap().to_string();
        let backend = SidecarBackend::new(dir.path().to_path_buf());

        // -- (1) Delete getName() (no-arg) — STRICT ------------------------
        let del_getter = JavaRewriteOp::DeleteMember {
            target_file: target_str.clone(),
            target_type: "Bean".into(),
            member_name: "getName".into(),
            parameter_types: Some(vec![]),
        };
        let bes = backend.rewrite(&del_getter).expect("deleteMember must succeed");
        assert_eq!(bes.file_edits.len(), 1, "deleting getName must produce one edit");
        let rewritten = bes.file_edits[0].new_text.clone().unwrap();
        assert!(
            !rewritten.contains("getName"),
            "getName must be removed; got:\n{rewritten}"
        );
        // Siblings preserved.
        assert!(
            rewritten.contains("setName") && rewritten.contains("private String name;"),
            "sibling members must be preserved; got:\n{rewritten}"
        );

        // -- (2) Idempotency: deleting an absent member is a no-op ---------
        std::fs::write(&target_file, &rewritten).expect("write rewritten back");
        let bes2 = backend.rewrite(&del_getter).expect("second deleteMember must succeed");
        assert!(
            bes2.file_edits.is_empty(),
            "deleting an already-absent member must be a no-op; got {} edit(s)",
            bes2.file_edits.len()
        );

        // -- (3) Ambiguity: name-only match on an overloaded name fails ----
        let overloaded_file = dir.path().join("Over.java");
        std::fs::write(
            &overloaded_file,
            "package com.example;\n\npublic class Over {\n    \
             public void run() {}\n    \
             public void run(int n) {}\n}\n",
        )
        .expect("write overloaded file");
        let del_ambiguous = JavaRewriteOp::DeleteMember {
            target_file: overloaded_file.to_str().unwrap().to_string(),
            target_type: "Over".into(),
            member_name: "run".into(),
            parameter_types: None, // name-only → ambiguous
        };
        let err = backend.rewrite(&del_ambiguous).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.member_ambiguous"),
            "name-only delete of an overloaded method must fail with error.member_ambiguous; \
             got: {msg}"
        );

        // -- (4) Same overload, disambiguated by parameter_types, succeeds -
        let del_specific = JavaRewriteOp::DeleteMember {
            target_file: overloaded_file.to_str().unwrap().to_string(),
            target_type: "Over".into(),
            member_name: "run".into(),
            parameter_types: Some(vec!["int".into()]),
        };
        let bes3 = backend.rewrite(&del_specific).expect("disambiguated delete must succeed");
        let rewritten3 = bes3.file_edits[0].new_text.clone().unwrap();
        assert!(
            rewritten3.contains("run()") && !rewritten3.contains("run(int n)"),
            "only run(int) must be removed; got:\n{rewritten3}"
        );
    }

    /// End-to-end proof for `InsertFieldAnnotation` (Phase 6, gap 3a). Strict:
    /// annotates the named field only, adds the import, is idempotent on re-run.
    #[test]
    fn live_insert_field_annotation_adds_then_idempotent() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live insertFieldAnnotation test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!(
                "[sidecar_backend] worker JAR missing — skipping live insertFieldAnnotation test"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("Bean.java");
        std::fs::write(
            &target_file,
            "package com.example;\n\npublic class Bean {\n    \
             private String name;\n    private int count;\n}\n",
        )
        .expect("write temp .java file");
        let target_str = target_file.to_str().unwrap().to_string();
        let backend = SidecarBackend::new(dir.path().to_path_buf());

        let op = JavaRewriteOp::InsertFieldAnnotation {
            target_file: target_str.clone(),
            target_type: "Bean".into(),
            field_name: "name".into(),
            annotation_text: "@Getter".into(),
            imports: vec!["lombok.Getter".into()],
        };

        let bes = backend.rewrite(&op).expect("insertFieldAnnotation must succeed");
        assert_eq!(bes.file_edits.len(), 1, "must produce one edit");
        let rewritten = bes.file_edits[0].new_text.clone().unwrap();
        assert!(
            rewritten.contains("@Getter"),
            "rewritten must contain @Getter; got:\n{rewritten}"
        );
        assert!(
            rewritten.contains("import lombok.Getter;"),
            "rewritten must contain the lombok.Getter import; got:\n{rewritten}"
        );
        // The annotation must attach to `name`, not `count`: @Getter appears
        // before `name` and there must be exactly one @Getter.
        assert_eq!(
            rewritten.matches("@Getter").count(),
            1,
            "exactly one @Getter (only the `name` field); got:\n{rewritten}"
        );
        let getter_at = rewritten.find("@Getter").unwrap();
        let name_at = rewritten.find("String name").unwrap();
        let count_at = rewritten.find("int count").unwrap();
        assert!(
            getter_at < name_at && getter_at < count_at,
            "@Getter must precede the `name` field; got:\n{rewritten}"
        );

        // Idempotency: re-run on annotated source is a no-op.
        std::fs::write(&target_file, &rewritten).expect("write rewritten back");
        let bes2 = backend.rewrite(&op).expect("second run must succeed");
        assert!(
            bes2.file_edits.is_empty(),
            "second run on annotated field must be a no-op; got {} edit(s)",
            bes2.file_edits.len()
        );
    }

    /// End-to-end proof for `PruneUnusedImport` (Phase 6, gap 3b). Strict: an
    /// unreferenced import is removed; a still-referenced import is kept (no-op).
    #[test]
    fn live_prune_unused_import_removes_unreferenced_keeps_referenced() {
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR unset — \
                 skipping live pruneUnusedImport test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!("[sidecar_backend] worker JAR missing — skipping live pruneUnusedImport test");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let backend = SidecarBackend::new(dir.path().to_path_buf());

        // -- Unreferenced import → removed --------------------------------
        let unused_file = dir.path().join("Unused.java");
        std::fs::write(
            &unused_file,
            "package com.example;\n\nimport java.util.List;\n\npublic class Unused {\n    \
             private int x;\n}\n",
        )
        .expect("write unused-import file");
        let op_unused = JavaRewriteOp::PruneUnusedImport {
            target_file: unused_file.to_str().unwrap().to_string(),
            imports: vec!["java.util.List".into()],
        };
        let bes = backend.rewrite(&op_unused).expect("prune must succeed");
        assert_eq!(bes.file_edits.len(), 1, "unreferenced import must be removed");
        let rewritten = bes.file_edits[0].new_text.clone().unwrap();
        assert!(
            !rewritten.contains("import java.util.List;"),
            "the unused import must be gone; got:\n{rewritten}"
        );

        // -- Still-referenced import → kept (no-op) -----------------------
        let used_file = dir.path().join("Used.java");
        std::fs::write(
            &used_file,
            "package com.example;\n\nimport java.util.List;\n\npublic class Used {\n    \
             private List<String> items;\n}\n",
        )
        .expect("write used-import file");
        let op_used = JavaRewriteOp::PruneUnusedImport {
            target_file: used_file.to_str().unwrap().to_string(),
            imports: vec!["java.util.List".into()],
        };
        let bes2 = backend.rewrite(&op_used).expect("prune must succeed");
        assert!(
            bes2.file_edits.is_empty(),
            "a still-referenced import must be kept (no-op); got {} edit(s)",
            bes2.file_edits.len()
        );
    }
}
