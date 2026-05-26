package com.blackbox.javaworker;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.annotation.JsonNaming;
import java.util.List;
import java.util.Objects;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class RpcRequest {
    private final long id;
    private final String method;
    private final JsonNode params;

    @JsonCreator
    public RpcRequest(
            @JsonProperty("id") long id,
            @JsonProperty("method") String method,
            @JsonProperty("params") JsonNode params) {
        this.id = id;
        this.method = method;
        this.params = params;
    }

    public long getId() { return id; }
    public String getMethod() { return method; }
    public JsonNode getParams() { return params; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class RpcResponse {
    /** JSON-RPC protocol version. Must be "2.0" in every response. */
    private static final String JSONRPC_VERSION = "2.0";

    private final String jsonrpc = JSONRPC_VERSION;
    private final long id;
    private final JsonNode result;
    private final RpcError error;

    private RpcResponse(long id, JsonNode result, RpcError error) {
        this.id = id;
        this.result = result;
        this.error = error;
    }

    public static RpcResponse success(long id, JsonNode result) {
        return new RpcResponse(id, result, null);
    }

    public static RpcResponse error(long id, int code, String message, String data) {
        return new RpcResponse(id, null, new RpcError(code, message, data));
    }

    public static RpcResponse error(long id, int code, String message, JsonNode data) {
        return new RpcResponse(id, null, new RpcError(code, message, data));
    }

    /** Always {@code "2.0"} — included in every response per JSON-RPC 2.0 spec. */
    public String getJsonrpc() { return jsonrpc; }
    public long getId() { return id; }
    public JsonNode getResult() { return result; }
    public RpcError getError() { return error; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class RpcError {
    private final int code;
    private final String message;
    private final JsonNode data;

    @JsonCreator
    public RpcError(
            @JsonProperty("code") int code,
            @JsonProperty("message") String message,
            @JsonProperty("data") JsonNode data) {
        this.code = code;
        this.message = message;
        this.data = data;
    }

    public RpcError(int code, String message, String data) {
        this.code = code;
        this.message = message;
        this.data = null;
    }

    public int getCode() { return code; }
    public String getMessage() { return message; }
    public JsonNode getData() { return data; }
}

// ---------------------------------------------------------------------------
// Method names
// ---------------------------------------------------------------------------

final class Methods {
    public static final String GET_CAPABILITIES = "getCapabilities";
    public static final String EMIT_TYPE = "emitType";
    public static final String INSERT_MEMBER = "insertMember";
    public static final String REPLACE_METHOD_BODY = "replaceMethodBody";
    public static final String INSERT_STATEMENT_IN_METHOD = "insertStatementInMethod";
    public static final String SHUTDOWN = "shutdown";

    private Methods() {}
}

// ---------------------------------------------------------------------------
// getCapabilities
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class CapabilitiesResult {
    private final int protocolVersion;
    private final String workerVersion;
    private final String javaVersion;
    private final String openrewriteVersion;
    private final List<String> supportedOps;

    public CapabilitiesResult(
            @JsonProperty("protocol_version") int protocolVersion,
            @JsonProperty("worker_version") String workerVersion,
            @JsonProperty("java_version") String javaVersion,
            @JsonProperty("openrewrite_version") String openrewriteVersion,
            @JsonProperty("supported_ops") List<String> supportedOps) {
        this.protocolVersion = protocolVersion;
        this.workerVersion = workerVersion;
        this.javaVersion = javaVersion;
        this.openrewriteVersion = openrewriteVersion;
        this.supportedOps = supportedOps;
    }

    public int getProtocolVersion() { return protocolVersion; }
    public String getWorkerVersion() { return workerVersion; }
    public String getJavaVersion() { return javaVersion; }
    public String getOpenrewriteVersion() { return openrewriteVersion; }
    public List<String> getSupportedOps() { return supportedOps; }
}

// ---------------------------------------------------------------------------
// emitType
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class EmitTypeParams {
    private final String sourceRoot;
    private final String packageName;
    private final String name;
    private final String kind;
    private final String sourceText;

    @JsonCreator
    public EmitTypeParams(
            @JsonProperty("source_root") String sourceRoot,
            @JsonProperty("package") String packageName,
            @JsonProperty("name") String name,
            @JsonProperty("kind") String kind,
            @JsonProperty("source_text") String sourceText) {
        this.sourceRoot = Objects.requireNonNull(sourceRoot, "source_root is required");
        this.packageName = Objects.requireNonNull(packageName, "package is required");
        this.name = Objects.requireNonNull(name, "name is required");
        this.kind = Objects.requireNonNull(kind, "kind is required");
        this.sourceText = sourceText;
    }

    public String getSourceRoot() { return sourceRoot; }
    public String getPackageName() { return packageName; }
    public String getName() { return name; }
    public String getKind() { return kind; }
    public String getSourceText() { return sourceText; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class FileCreate {
    private final String path;
    private final String content;

    @JsonCreator
    public FileCreate(
            @JsonProperty("path") String path,
            @JsonProperty("content") String content) {
        this.path = path;
        this.content = content;
    }

    public String getPath() { return path; }
    public String getContent() { return content; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class SidecarDiagnostic {
    private final String severity;
    private final String message;
    private final Integer line;
    private final Integer column;

    @JsonCreator
    public SidecarDiagnostic(
            @JsonProperty("severity") String severity,
            @JsonProperty("message") String message,
            @JsonProperty("line") Integer line,
            @JsonProperty("column") Integer column) {
        this.severity = severity;
        this.message = message;
        this.line = line;
        this.column = column;
    }

    public String getSeverity() { return severity; }
    public String getMessage() { return message; }
    public Integer getLine() { return line; }
    public Integer getColumn() { return column; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class EmitTypeResult {
    private final List<FileCreate> fileCreates;
    /** Non-fatal advisory messages (strings). Errors surface as JSON-RPC error responses. */
    private final List<String> diagnostics;

    @JsonCreator
    public EmitTypeResult(
            @JsonProperty("file_creates") List<FileCreate> fileCreates,
            @JsonProperty("diagnostics") List<String> diagnostics) {
        this.fileCreates = fileCreates;
        this.diagnostics = diagnostics;
    }

    public List<FileCreate> getFileCreates() { return fileCreates; }
    public List<String> getDiagnostics() { return diagnostics; }
}

// ---------------------------------------------------------------------------
// insertMember
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class InsertMemberParams {
    private final String targetFile;
    private final String sourceText;
    private final String targetType;
    private final String memberText;
    private final List<String> imports;

    @JsonCreator
    public InsertMemberParams(
            @JsonProperty("target_file") String targetFile,
            @JsonProperty("source_text") String sourceText,
            @JsonProperty("target_type") String targetType,
            @JsonProperty("member_text") String memberText,
            @JsonProperty("imports") List<String> imports) {
        this.targetFile = Objects.requireNonNull(targetFile, "target_file is required");
        this.sourceText = Objects.requireNonNull(sourceText, "source_text is required");
        this.targetType = Objects.requireNonNull(targetType, "target_type is required");
        this.memberText = Objects.requireNonNull(memberText, "member_text is required");
        this.imports = imports;
    }

    public String getTargetFile() { return targetFile; }
    public String getSourceText() { return sourceText; }
    public String getTargetType() { return targetType; }
    public String getMemberText() { return memberText; }
    public List<String> getImports() { return imports; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class InsertMemberResult {
    private final String rewrittenSource;
    private final boolean changed;
    private final boolean noOp;
    /** Non-fatal advisory messages (strings). Errors surface as JSON-RPC error responses. */
    private final List<String> diagnostics;

    @JsonCreator
    public InsertMemberResult(
            @JsonProperty("rewritten_source") String rewrittenSource,
            @JsonProperty("changed") boolean changed,
            @JsonProperty("no_op") boolean noOp,
            @JsonProperty("diagnostics") List<String> diagnostics) {
        this.rewrittenSource = rewrittenSource;
        this.changed = changed;
        this.noOp = noOp;
        this.diagnostics = diagnostics;
    }

    public static InsertMemberResult changed(String rewrittenSource) {
        return new InsertMemberResult(rewrittenSource, true, false, List.of());
    }

    /** No-op: member already exists; {@code reason} is an advisory string. */
    public static InsertMemberResult noOp(String originalSource, String reason) {
        return new InsertMemberResult(originalSource, false, true, List.of(reason));
    }

    public String getRewrittenSource() { return rewrittenSource; }
    public boolean isChanged() { return changed; }
    public boolean isNoOp() { return noOp; }
    public List<String> getDiagnostics() { return diagnostics; }
}

// ---------------------------------------------------------------------------
// replaceMethodBody
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class ReplaceMethodBodyParams {
    private final String targetFile;
    private final String sourceText;
    private final String targetType;
    private final String methodName;
    private final List<String> parameterTypes;
    private final String newBody;
    private final List<String> imports;

    @JsonCreator
    public ReplaceMethodBodyParams(
            @JsonProperty("target_file") String targetFile,
            @JsonProperty("source_text") String sourceText,
            @JsonProperty("target_type") String targetType,
            @JsonProperty("method_name") String methodName,
            @JsonProperty("parameter_types") List<String> parameterTypes,
            @JsonProperty("new_body") String newBody,
            @JsonProperty("imports") List<String> imports) {
        this.targetFile = Objects.requireNonNull(targetFile, "target_file is required");
        this.sourceText = Objects.requireNonNull(sourceText, "source_text is required");
        this.targetType = Objects.requireNonNull(targetType, "target_type is required");
        this.methodName = Objects.requireNonNull(methodName, "method_name is required");
        this.parameterTypes = parameterTypes != null ? parameterTypes : List.of();
        this.newBody = Objects.requireNonNull(newBody, "new_body is required");
        this.imports = imports != null ? imports : List.of();
    }

    public String getTargetFile() { return targetFile; }
    public String getSourceText() { return sourceText; }
    public String getTargetType() { return targetType; }
    public String getMethodName() { return methodName; }
    public List<String> getParameterTypes() { return parameterTypes; }
    public String getNewBody() { return newBody; }
    public List<String> getImports() { return imports; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class ReplaceMethodBodyResult {
    private final String rewrittenSource;
    private final boolean changed;
    private final boolean noOp;
    private final List<String> diagnostics;

    @JsonCreator
    public ReplaceMethodBodyResult(
            @JsonProperty("rewritten_source") String rewrittenSource,
            @JsonProperty("changed") boolean changed,
            @JsonProperty("no_op") boolean noOp,
            @JsonProperty("diagnostics") List<String> diagnostics) {
        this.rewrittenSource = rewrittenSource;
        this.changed = changed;
        this.noOp = noOp;
        this.diagnostics = diagnostics != null ? diagnostics : List.of();
    }

    public static ReplaceMethodBodyResult changed(String rewrittenSource) {
        return new ReplaceMethodBodyResult(rewrittenSource, true, false, List.of());
    }

    public static ReplaceMethodBodyResult noOp(String originalSource, String reason) {
        return new ReplaceMethodBodyResult(originalSource, false, true, List.of(reason));
    }

    public String getRewrittenSource() { return rewrittenSource; }
    public boolean isChanged() { return changed; }
    public boolean isNoOp() { return noOp; }
    public List<String> getDiagnostics() { return diagnostics; }
}

// ---------------------------------------------------------------------------
// insertStatementInMethod
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class InsertStatementInMethodParams {
    private final String targetFile;
    private final String sourceText;
    private final String targetType;
    private final String methodName;
    private final List<String> parameterTypes;
    private final String statementText;
    private final String placement;
    private final List<String> imports;

    @JsonCreator
    public InsertStatementInMethodParams(
            @JsonProperty("target_file") String targetFile,
            @JsonProperty("source_text") String sourceText,
            @JsonProperty("target_type") String targetType,
            @JsonProperty("method_name") String methodName,
            @JsonProperty("parameter_types") List<String> parameterTypes,
            @JsonProperty("statement_text") String statementText,
            @JsonProperty("placement") String placement,
            @JsonProperty("imports") List<String> imports) {
        this.targetFile = Objects.requireNonNull(targetFile, "target_file is required");
        this.sourceText = Objects.requireNonNull(sourceText, "source_text is required");
        this.targetType = Objects.requireNonNull(targetType, "target_type is required");
        this.methodName = Objects.requireNonNull(methodName, "method_name is required");
        this.parameterTypes = parameterTypes != null ? parameterTypes : List.of();
        this.statementText = Objects.requireNonNull(statementText, "statement_text is required");
        this.placement = placement != null ? placement : "append";
        this.imports = imports != null ? imports : List.of();
    }

    public String getTargetFile() { return targetFile; }
    public String getSourceText() { return sourceText; }
    public String getTargetType() { return targetType; }
    public String getMethodName() { return methodName; }
    public List<String> getParameterTypes() { return parameterTypes; }
    public String getStatementText() { return statementText; }
    public String getPlacement() { return placement; }
    public List<String> getImports() { return imports; }
}

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class InsertStatementInMethodResult {
    private final String rewrittenSource;
    private final boolean changed;
    private final boolean noOp;
    private final List<String> diagnostics;

    @JsonCreator
    public InsertStatementInMethodResult(
            @JsonProperty("rewritten_source") String rewrittenSource,
            @JsonProperty("changed") boolean changed,
            @JsonProperty("no_op") boolean noOp,
            @JsonProperty("diagnostics") List<String> diagnostics) {
        this.rewrittenSource = rewrittenSource;
        this.changed = changed;
        this.noOp = noOp;
        this.diagnostics = diagnostics != null ? diagnostics : List.of();
    }

    public static InsertStatementInMethodResult changed(String rewrittenSource) {
        return new InsertStatementInMethodResult(rewrittenSource, true, false, List.of());
    }

    public static InsertStatementInMethodResult noOp(String originalSource, String reason) {
        return new InsertStatementInMethodResult(originalSource, false, true, List.of(reason));
    }

    public String getRewrittenSource() { return rewrittenSource; }
    public boolean isChanged() { return changed; }
    public boolean isNoOp() { return noOp; }
    public List<String> getDiagnostics() { return diagnostics; }
}

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class ShutdownResult {
    private final boolean ok;

    @JsonCreator
    public ShutdownResult(@JsonProperty("ok") boolean ok) {
        this.ok = ok;
    }

    public boolean isOk() { return ok; }
}
