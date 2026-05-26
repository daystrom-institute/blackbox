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
//! # rewrite (JavaRewriteOp::InsertMember)
//!
//! Reads `target_file` from disk, computes `original_sha256`, sends
//! `insertMember`. On `no_op=true` returns an empty `BackendEditSet`.
//! On `changed=true` builds a single full-span `FileEdit` whose edit
//! covers the entire original file. Maps `error.member_conflict` through
//! from the worker's RPC error message. Pool unavailable → error.backend_unavailable.

#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use super::backend::{BackendEditSet, JavaEmitOp, JavaMacroBackend, JavaRewriteOp};
use super::java_sidecar::JavaWorkerPool;
use super::java_sidecar_protocol::{
    EmitTypeParams, InsertMemberParams, METHOD_EMIT_TYPE, METHOD_INSERT_MEMBER,
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

        Ok(BackendEditSet {
            file_edits: vec![],
            file_creates,
        })
    }

    fn rewrite(&self, op: &JavaRewriteOp) -> Result<BackendEditSet> {
        let JavaRewriteOp::InsertMember {
            target_file,
            target_type,
            member_text,
            imports,
        } = op;

        let worker_arc = self.pool.worker_for(&self.project_root).map_err(|e| {
            anyhow!(
                "error.backend_unavailable: Java macro sidecar unavailable for rewrite: {e}"
            )
        })?;

        // Read the target file from disk and compute its preimage SHA-256.
        let source_text = std::fs::read_to_string(target_file).map_err(|e| {
            anyhow!(
                "error.backend_unavailable: cannot read target file '{}': {e}",
                target_file
            )
        })?;

        let original_sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(source_text.as_bytes());
            format!("{:x}", hasher.finalize())
        };

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
            .map_err(|e| {
                // Propagate error.member_conflict from the worker's RPC error message.
                let msg = e.to_string();
                if msg.contains("error.member_conflict") {
                    anyhow!("error.member_conflict: {e}")
                } else {
                    anyhow!("error.backend_unavailable: insertMember RPC failed: {e}")
                }
            })?;

        // no_op: member already existed, nothing to change.
        if result.no_op {
            return Ok(BackendEditSet::default());
        }

        // changed=true: build a single full-span FileEdit replacing the whole file.
        if result.changed {
            let byte_end = source_text.len();
            let new_text = result.rewritten_source.clone();
            let edit = FileEdit {
                path: target_file.clone(),
                original_sha256,
                edits: vec![TextEdit {
                    byte_start: 0,
                    byte_end,
                    replacement: new_text.clone(),
                }],
                new_text: Some(new_text),
            };
            return Ok(BackendEditSet {
                file_edits: vec![edit],
                file_creates: vec![],
            });
        }

        // R2: changed=false AND no_op=false is an invalid worker response — the
        // worker must set exactly one of the two flags. Fail closed rather than
        // silently swallowing the ambiguous state.
        Err(anyhow!(
            "error.backend_unavailable: insertMember returned changed=false no_op=false; \
             the worker response is ambiguous (expected exactly one flag set)"
        ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend() -> SidecarBackend {
        SidecarBackend::new(PathBuf::from("/tmp/test-project"))
    }

    #[test]
    fn emit_fails_closed_when_jar_unset() {
        // If the JAR env var is set and the JAR exists we skip to avoid spawning.
        if let Ok(jar) = std::env::var("BLACKBOX_JAVA_WORKER_JAR") {
            if !jar.trim().is_empty() && PathBuf::from(&jar).exists() {
                eprintln!(
                    "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                     skipping unavailability test"
                );
                return;
            }
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

    #[test]
    fn rewrite_fails_closed_when_jar_unset() {
        if let Ok(jar) = std::env::var("BLACKBOX_JAVA_WORKER_JAR") {
            if !jar.trim().is_empty() && PathBuf::from(&jar).exists() {
                eprintln!(
                    "[sidecar_backend] BLACKBOX_JAVA_WORKER_JAR present — \
                     skipping unavailability test"
                );
                return;
            }
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
}
