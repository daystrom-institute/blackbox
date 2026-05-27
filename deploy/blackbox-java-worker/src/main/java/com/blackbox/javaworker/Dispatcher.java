package com.blackbox.javaworker;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;

/**
 * Routes incoming JSON-RPC requests to the appropriate handler and
 * serialises the response.
 *
 * <p>Filesystem-free: all state is in-memory and per-request.
 */
final class Dispatcher {

    private final ObjectMapper mapper;
    private final EmitTypeHandler emitTypeHandler;
    private final InsertMemberHandler insertMemberHandler;
    private final ReplaceMethodBodyHandler replaceMethodBodyHandler;
    private final InsertStatementInMethodHandler insertStatementInMethodHandler;
    private final InsertClassAnnotationHandler insertClassAnnotationHandler;
    private final DeleteMemberHandler deleteMemberHandler;
    private final InsertFieldAnnotationHandler insertFieldAnnotationHandler;
    private final PruneUnusedImportHandler pruneUnusedImportHandler;
    private final AnalyzeClassHandler analyzeClassHandler;

    Dispatcher(ObjectMapper mapper) {
        this.mapper = mapper;
        this.emitTypeHandler = new EmitTypeHandler();
        this.insertMemberHandler = new InsertMemberHandler();
        this.replaceMethodBodyHandler = new ReplaceMethodBodyHandler();
        this.insertStatementInMethodHandler = new InsertStatementInMethodHandler();
        this.insertClassAnnotationHandler = new InsertClassAnnotationHandler();
        this.deleteMemberHandler = new DeleteMemberHandler();
        this.insertFieldAnnotationHandler = new InsertFieldAnnotationHandler();
        this.pruneUnusedImportHandler = new PruneUnusedImportHandler();
        this.analyzeClassHandler = new AnalyzeClassHandler();
    }

    RpcResponse dispatch(RpcRequest request) {
        if (request.getMethod() == null) {
            return error(request.getId(), -32600, "Invalid Request: missing method");
        }

        try {
            switch (request.getMethod()) {
                case Methods.GET_CAPABILITIES:
                    return handleGetCapabilities(request);
                case Methods.EMIT_TYPE:
                    return handleEmitType(request);
                case Methods.INSERT_MEMBER:
                    return handleInsertMember(request);
                case Methods.REPLACE_METHOD_BODY:
                    return handleReplaceMethodBody(request);
                case Methods.INSERT_STATEMENT_IN_METHOD:
                    return handleInsertStatementInMethod(request);
                case Methods.INSERT_CLASS_ANNOTATION:
                    return handleInsertClassAnnotation(request);
                case Methods.DELETE_MEMBER:
                    return handleDeleteMember(request);
                case Methods.INSERT_FIELD_ANNOTATION:
                    return handleInsertFieldAnnotation(request);
                case Methods.PRUNE_UNUSED_IMPORT:
                    return handlePruneUnusedImport(request);
                case Methods.ANALYZE_CLASS:
                    return handleAnalyzeClass(request);
                case Methods.SHUTDOWN:
                    return handleShutdown(request);
                default:
                    return error(request.getId(), -32601,
                            "Method not found: " + request.getMethod());
            }
        } catch (SidecarException e) {
            // Semantic failure from a handler (parse_invalid, member_conflict, type_mismatch).
            // Use the handler-specific JSON-RPC error code rather than the generic -32603.
            return error(request.getId(), e.getCode(), e.getMessage());
        } catch (IllegalArgumentException e) {
            // Validation errors from @JsonCreator or Objects.requireNonNull
            return error(request.getId(), -32602,
                    "Invalid params: " + e.getMessage());
        } catch (Exception e) {
            return error(request.getId(), -32603,
                    "Internal error: " + e.getMessage());
        }
    }

    // -- handlers ----------------------------------------------------------

    private RpcResponse handleGetCapabilities(RpcRequest request) {
        // Java version from system property
        String javaVersion = System.getProperty("java.version", "unknown");

        // OpenRewrite version from package metadata
        String openrewriteVersion = OpenRewriteVersion.get();

        CapabilitiesResult caps = new CapabilitiesResult(
                1,                              // protocol_version
                "1.0.0",                        // worker_version
                javaVersion,
                openrewriteVersion,
                java.util.List.of(
                        "emit_type",
                        "insert_member",
                        "replace_method_body",
                        "insert_statement_in_method",
                        "insert_class_annotation",
                        "delete_member",
                        "insert_field_annotation",
                        "prune_unused_import",
                        "analyze_class"));

        return success(request.getId(), caps);
    }

    private RpcResponse handleEmitType(RpcRequest request) {
        EmitTypeParams params = deserializeParams(request, EmitTypeParams.class);
        EmitTypeResult result = emitTypeHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleInsertMember(RpcRequest request) {
        InsertMemberParams params = deserializeParams(request, InsertMemberParams.class);
        InsertMemberResult result = insertMemberHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleReplaceMethodBody(RpcRequest request) {
        ReplaceMethodBodyParams params = deserializeParams(request, ReplaceMethodBodyParams.class);
        ReplaceMethodBodyResult result = replaceMethodBodyHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleInsertStatementInMethod(RpcRequest request) {
        InsertStatementInMethodParams params =
                deserializeParams(request, InsertStatementInMethodParams.class);
        InsertStatementInMethodResult result = insertStatementInMethodHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleInsertClassAnnotation(RpcRequest request) {
        InsertClassAnnotationParams params =
                deserializeParams(request, InsertClassAnnotationParams.class);
        InsertClassAnnotationResult result = insertClassAnnotationHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleDeleteMember(RpcRequest request) {
        DeleteMemberParams params = deserializeParams(request, DeleteMemberParams.class);
        DeleteMemberResult result = deleteMemberHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleInsertFieldAnnotation(RpcRequest request) {
        InsertFieldAnnotationParams params =
                deserializeParams(request, InsertFieldAnnotationParams.class);
        InsertFieldAnnotationResult result = insertFieldAnnotationHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handlePruneUnusedImport(RpcRequest request) {
        PruneUnusedImportParams params =
                deserializeParams(request, PruneUnusedImportParams.class);
        PruneUnusedImportResult result = pruneUnusedImportHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleAnalyzeClass(RpcRequest request) {
        AnalyzeClassParams params = deserializeParams(request, AnalyzeClassParams.class);
        AnalyzeClassResult result = analyzeClassHandler.handle(params);
        return success(request.getId(), result);
    }

    private RpcResponse handleShutdown(RpcRequest request) {
        return success(request.getId(), new ShutdownResult(true));
    }

    // -- helpers -----------------------------------------------------------

    private <T> T deserializeParams(RpcRequest request, Class<T> type) {
        if (request.getParams() == null || request.getParams().isNull()) {
            // Create empty params object for methods that don't require params
            try {
                return mapper.treeToValue(mapper.createObjectNode(), type);
            } catch (JsonProcessingException e) {
                throw new IllegalArgumentException("Unable to create default params: " + e.getMessage(), e);
            }
        }
        try {
            return mapper.treeToValue(request.getParams(), type);
        } catch (JsonProcessingException e) {
            throw new IllegalArgumentException(
                    "Unable to deserialize params for " + request.getMethod() + ": " + e.getMessage(), e);
        }
    }

    private RpcResponse success(long id, Object result) {
        return RpcResponse.success(id, mapper.valueToTree(result));
    }

    private RpcResponse error(long id, int code, String message) {
        return RpcResponse.error(id, code, message, (String) null);
    }
}
