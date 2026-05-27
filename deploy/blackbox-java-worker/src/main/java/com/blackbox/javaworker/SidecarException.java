package com.blackbox.javaworker;

/**
 * Thrown by handler implementations when a request cannot be fulfilled due to a
 * semantic error in the input.
 *
 * <p>The {@link Dispatcher} catches this exception and converts it to a JSON-RPC
 * error response using the handler-specific {@link #code}. Unexpected
 * {@link RuntimeException}s are caught separately and mapped to the generic
 * {@code -32603 Internal error} code.
 *
 * <h3>Error codes</h3>
 * <ul>
 *   <li>{@link #PARSE_INVALID} ({@code -32001}) — source text failed to parse or
 *       the supplied kind/name/package did not match the parsed content.</li>
 *   <li>{@link #MEMBER_CONFLICT} ({@code -32002}) — a member with the same
 *       signature already exists in the target type but has a different body.</li>
 *   <li>{@link #TYPE_MISMATCH} ({@code -32003}) — the source_text declares a
 *       type that does not match the expected name, kind, or package.</li>
 * </ul>
 */
final class SidecarException extends RuntimeException {

    /** Source text failed to parse, or name/kind/package validation failed. */
    static final int PARSE_INVALID = -32001;

    /** Member with same signature exists but has a different body — true conflict. */
    static final int MEMBER_CONFLICT = -32002;

    /** Declared package, name, or kind in source_text does not match params. */
    static final int TYPE_MISMATCH = -32003;

    /** No method matching the given name and parameter types was found in the target type. */
    static final int METHOD_NOT_FOUND = -32004;

    /** Multiple methods match the given name and parameter types — the combination is ambiguous. */
    static final int METHOD_AMBIGUOUS = -32005;

    /** A name-only deleteMember matched more than one member — supply parameter_types. */
    static final int MEMBER_AMBIGUOUS = -32006;

    private final int code;

    SidecarException(int code, String message) {
        super(message);
        this.code = code;
    }

    /** JSON-RPC error code for this exception. */
    int getCode() {
        return code;
    }
}
