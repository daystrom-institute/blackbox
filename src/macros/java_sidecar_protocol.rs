//! JSON-RPC protocol types for the Java macro sidecar
//! (`blackbox-java-worker`).
//!
//! The sidecar is a small JVM process (OpenRewrite + JavaPoet) that handles
//! Java source generation and rewrite operations and communicates over stdio.
//! The protocol uses JSON-RPC 2.0 framing — each request is a JSON object on
//! its own line, each response is the matching JSON object with the same `id`.
//!
//! Method names are camelCase to match LSP idiom. Per-message shapes match
//! the design doc (`design/refactor-tools/unified-code-synthesis-model.md`,
//! "Java Backend section").
//!
//! This module is **types only** — the actual transport / spawn / lifecycle
//! is in `src/macros/java_sidecar.rs`.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ── JSON-RPC envelope ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest<P> {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse<R> {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ── getCapabilities ──────────────────────────────────────────────────────────

/// `getCapabilities()` request: no parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCapabilitiesParams {}

/// `getCapabilities()` result: worker identity and supported operations.
///
/// `protocol_version` must equal 1 for this client to accept the worker.
/// Fail-closed: any other value causes spawn to abort (RX-V3 mirror).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCapabilitiesResult {
    /// JSON-RPC protocol version spoken by the worker. Must be 1.
    pub protocol_version: u32,
    /// Worker implementation version string (e.g. `"0.1.0"`).
    pub worker_version: String,
    /// JVM version string (e.g. `"21.0.3"`).
    pub java_version: String,
    /// OpenRewrite version string (e.g. `"8.42.1"`).
    pub openrewrite_version: String,
    /// List of operation names this worker supports,
    /// e.g. `["emitType", "insertMember"]`.
    pub supported_ops: Vec<String>,
}

// ── emitType ─────────────────────────────────────────────────────────────────

/// `emitType()` request: emit a new top-level Java type as a new source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitTypeParams {
    /// Absolute path to the Java source root (e.g. `"src/main/java"`).
    pub source_root: String,
    /// Java package, e.g. `"com.example.payments"`.
    pub package: String,
    /// Simple type name, e.g. `"PaymentService"`.
    pub name: String,
    /// Type kind: `"class"`, `"interface"`, `"enum"`, `"annotation"`, or `"record"`.
    pub kind: String,
    /// Full source text of the new type (including package declaration and imports).
    /// Worker validates parse-validity before returning.
    pub source_text: String,
}

/// `emitType()` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitTypeResult {
    /// Files created by this operation.
    pub file_creates: Vec<FileCreate>,
    /// Non-fatal diagnostic messages from the worker (e.g. parse warnings).
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

/// A file the worker created as part of an `emitType` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCreate {
    /// Absolute path where the file should be written.
    pub path: String,
    /// Full content of the new file.
    pub content: String,
}

// ── insertMember ─────────────────────────────────────────────────────────────

/// `insertMember()` request: insert a member into an existing type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertMemberParams {
    /// Absolute path to the file containing the target type.
    pub target_file: String,
    /// Full content of the target file as it currently exists on disk.
    /// Sent by the client so the worker's in-memory state stays coherent
    /// without requiring a filesystem read on the worker side.
    pub source_text: String,
    /// Simple name of the type to insert the member into.
    pub target_type: String,
    /// Source text of the member to insert (method signature + body,
    /// field declaration, etc.).
    pub member_text: String,
    /// Fully-qualified import statements to add if not already present.
    pub imports: Vec<String>,
}

/// `insertMember()` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertMemberResult {
    /// Full rewritten source text of the file after the member is inserted.
    /// Only meaningful when `changed` is true.
    pub rewritten_source: String,
    /// True when the source was actually modified.
    pub changed: bool,
    /// True when the member already existed and no insertion was needed.
    pub no_op: bool,
    /// Non-fatal diagnostic messages from the worker.
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ── replaceMethodBody ─────────────────────────────────────────────────────────

/// `replaceMethodBody()` request: replace an existing method's body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceMethodBodyParams {
    /// Absolute path to the file (for reference; not read by worker).
    pub target_file: String,
    /// Full content of the target file as it currently exists on disk.
    pub source_text: String,
    /// Simple name of the type containing the method.
    pub target_type: String,
    /// Simple name of the method to replace the body of.
    pub method_name: String,
    /// Written parameter types for disambiguation, e.g. `["String", "int"]`.
    pub parameter_types: Vec<String>,
    /// New body text (statements only, no surrounding braces).
    pub new_body: String,
    /// Fully-qualified imports to add if not already present.
    pub imports: Vec<String>,
}

/// `replaceMethodBody()` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceMethodBodyResult {
    /// Full rewritten source text of the file after the body is replaced.
    pub rewritten_source: String,
    /// True when the source was actually modified.
    pub changed: bool,
    /// True when the body already matched and no replacement was needed.
    pub no_op: bool,
    /// Non-fatal diagnostic messages from the worker.
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ── insertStatementInMethod ───────────────────────────────────────────────────

/// `insertStatementInMethod()` request: append statements to a method body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertStatementInMethodParams {
    /// Absolute path to the file (for reference; not read by worker).
    pub target_file: String,
    /// Full content of the target file as it currently exists on disk.
    pub source_text: String,
    /// Simple name of the type containing the method.
    pub target_type: String,
    /// Simple name of the method to insert statements into.
    pub method_name: String,
    /// Written parameter types for disambiguation.
    pub parameter_types: Vec<String>,
    /// Statement text to insert (one or more Java statements).
    pub statement_text: String,
    /// Insertion position. Must be `"append"` in v1.
    pub placement: String,
    /// Fully-qualified imports to add if not already present.
    pub imports: Vec<String>,
}

/// `insertStatementInMethod()` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertStatementInMethodResult {
    /// Full rewritten source text of the file after insertion.
    pub rewritten_source: String,
    /// True when the source was actually modified.
    pub changed: bool,
    /// True when the statement(s) already existed and no insertion was needed.
    pub no_op: bool,
    /// Non-fatal diagnostic messages from the worker.
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ── shutdown ─────────────────────────────────────────────────────────────────

/// `shutdown()` request: no parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownParams {}

// ── Method name constants ─────────────────────────────────────────────────────

pub const METHOD_GET_CAPABILITIES: &str = "getCapabilities";
pub const METHOD_EMIT_TYPE: &str = "emitType";
pub const METHOD_INSERT_MEMBER: &str = "insertMember";
pub const METHOD_REPLACE_METHOD_BODY: &str = "replaceMethodBody";
pub const METHOD_INSERT_STATEMENT_IN_METHOD: &str = "insertStatementInMethod";
pub const METHOD_SHUTDOWN: &str = "shutdown";

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_request_serializes_camel_case_method() {
        let req = RpcRequest::<GetCapabilitiesParams> {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: METHOD_GET_CAPABILITIES.to_string(),
            params: Some(GetCapabilitiesParams {}),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"getCapabilities\""), "{s}");
    }

    #[test]
    fn get_capabilities_result_round_trips() {
        let result = GetCapabilitiesResult {
            protocol_version: 1,
            worker_version: "0.1.0".to_string(),
            java_version: "21.0.3".to_string(),
            openrewrite_version: "8.42.1".to_string(),
            supported_ops: vec!["emitType".to_string(), "insertMember".to_string()],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: GetCapabilitiesResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.protocol_version, 1);
        assert_eq!(back.worker_version, "0.1.0");
        assert_eq!(back.supported_ops.len(), 2);
    }

    #[test]
    fn emit_type_params_round_trips() {
        let params = EmitTypeParams {
            source_root: "/repo/src/main/java".to_string(),
            package: "com.example.payments".to_string(),
            name: "PaymentService".to_string(),
            kind: "interface".to_string(),
            source_text: "package com.example.payments;\npublic interface PaymentService {}\n"
                .to_string(),
        };
        let s = serde_json::to_string(&params).unwrap();
        let back: EmitTypeParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "PaymentService");
        assert_eq!(back.kind, "interface");
        assert_eq!(back.source_root, "/repo/src/main/java");
    }

    #[test]
    fn emit_type_result_round_trips() {
        let result = EmitTypeResult {
            file_creates: vec![FileCreate {
                path: "/repo/src/main/java/com/example/payments/PaymentService.java".to_string(),
                content: "package com.example.payments;\npublic interface PaymentService {}\n"
                    .to_string(),
            }],
            diagnostics: vec![],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: EmitTypeResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.file_creates.len(), 1);
        assert_eq!(
            back.file_creates[0].path,
            "/repo/src/main/java/com/example/payments/PaymentService.java"
        );
    }

    #[test]
    fn insert_member_params_round_trips() {
        let params = InsertMemberParams {
            target_file: "/repo/src/main/java/com/example/FooImpl.java".to_string(),
            source_text: "package com.example;\npublic class FooImpl {}\n".to_string(),
            target_type: "FooImpl".to_string(),
            member_text: "public void doWork() {}".to_string(),
            imports: vec!["com.example.core.Port".to_string()],
        };
        let s = serde_json::to_string(&params).unwrap();
        let back: InsertMemberParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.target_type, "FooImpl");
        assert_eq!(back.imports.len(), 1);
        assert_eq!(back.imports[0], "com.example.core.Port");
    }

    #[test]
    fn insert_member_result_no_op_round_trips() {
        let result = InsertMemberResult {
            rewritten_source: String::new(),
            changed: false,
            no_op: true,
            diagnostics: vec![],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: InsertMemberResult = serde_json::from_str(&s).unwrap();
        assert!(back.no_op);
        assert!(!back.changed);
    }

    #[test]
    fn rpc_response_with_error_round_trips() {
        let resp = RpcResponse::<EmitTypeResult> {
            jsonrpc: "2.0".to_string(),
            id: 3,
            result: None,
            error: Some(RpcError {
                code: -32603,
                message: "error.member_conflict: member already exists".to_string(),
                data: None,
            }),
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: RpcResponse<EmitTypeResult> = serde_json::from_str(&s).unwrap();
        assert!(back.error.is_some());
        assert!(back.result.is_none());
        assert!(
            back.error
                .unwrap()
                .message
                .contains("error.member_conflict")
        );
    }

    #[test]
    fn method_constants_are_camel_case() {
        assert_eq!(METHOD_GET_CAPABILITIES, "getCapabilities");
        assert_eq!(METHOD_EMIT_TYPE, "emitType");
        assert_eq!(METHOD_INSERT_MEMBER, "insertMember");
        assert_eq!(METHOD_REPLACE_METHOD_BODY, "replaceMethodBody");
        assert_eq!(METHOD_INSERT_STATEMENT_IN_METHOD, "insertStatementInMethod");
        assert_eq!(METHOD_SHUTDOWN, "shutdown");
    }

    #[test]
    fn replace_method_body_params_round_trips() {
        let params = ReplaceMethodBodyParams {
            target_file: "/repo/src/FooImpl.java".to_string(),
            source_text: "package com.example;\npublic class FooImpl { void doWork(String s) {} }"
                .to_string(),
            target_type: "FooImpl".to_string(),
            method_name: "doWork".to_string(),
            parameter_types: vec!["String".to_string()],
            new_body: "System.out.println(s);".to_string(),
            imports: vec![],
        };
        let s = serde_json::to_string(&params).unwrap();
        let back: ReplaceMethodBodyParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.target_type, "FooImpl");
        assert_eq!(back.method_name, "doWork");
        assert_eq!(back.parameter_types, vec!["String"]);
        assert!(back.new_body.contains("println"));
    }

    #[test]
    fn replace_method_body_result_round_trips() {
        let result = ReplaceMethodBodyResult {
            rewritten_source: "package com.example;\npublic class FooImpl { void doWork(String s) { System.out.println(s); } }".to_string(),
            changed: true,
            no_op: false,
            diagnostics: vec![],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: ReplaceMethodBodyResult = serde_json::from_str(&s).unwrap();
        assert!(back.changed);
        assert!(!back.no_op);
        assert!(back.rewritten_source.contains("println"));
    }

    #[test]
    fn replace_method_body_result_no_op_round_trips() {
        let result = ReplaceMethodBodyResult {
            rewritten_source: "unchanged".to_string(),
            changed: false,
            no_op: true,
            diagnostics: vec!["body already matches".to_string()],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: ReplaceMethodBodyResult = serde_json::from_str(&s).unwrap();
        assert!(!back.changed);
        assert!(back.no_op);
        assert_eq!(back.diagnostics.len(), 1);
    }

    #[test]
    fn insert_statement_in_method_params_round_trips() {
        let params = InsertStatementInMethodParams {
            target_file: "/repo/src/FooImpl.java".to_string(),
            source_text: "package com.example;\npublic class FooImpl { void init() {} }"
                .to_string(),
            target_type: "FooImpl".to_string(),
            method_name: "init".to_string(),
            parameter_types: vec![],
            statement_text: "this.ready = true;".to_string(),
            placement: "append".to_string(),
            imports: vec![],
        };
        let s = serde_json::to_string(&params).unwrap();
        let back: InsertStatementInMethodParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method_name, "init");
        assert_eq!(back.placement, "append");
        assert!(back.statement_text.contains("ready"));
        assert!(back.parameter_types.is_empty());
    }

    #[test]
    fn insert_statement_in_method_result_round_trips() {
        let result = InsertStatementInMethodResult {
            rewritten_source: "package com.example;\npublic class FooImpl { void init() { this.ready = true; } }".to_string(),
            changed: true,
            no_op: false,
            diagnostics: vec![],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: InsertStatementInMethodResult = serde_json::from_str(&s).unwrap();
        assert!(back.changed);
        assert!(!back.no_op);
    }
}
