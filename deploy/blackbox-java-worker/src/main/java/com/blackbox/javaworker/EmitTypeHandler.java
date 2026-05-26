package com.blackbox.javaworker;

import java.util.List;
import java.util.Optional;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.tree.J;

/**
 * Handles the {@code emitType} JSON-RPC method.
 *
 * <p>Validates the macro-supplied {@code source_text} with OpenRewrite's parser
 * and returns a {@link FileCreate} whose content is the format-preserving
 * round-trip of that source (not a generated skeleton). The file path is
 * computed from {@code source_root}, {@code package}, and {@code name}.
 *
 * <p>Semantic failures are reported as {@link SidecarException}, which the
 * {@link Dispatcher} converts to a JSON-RPC error response. Only non-fatal
 * advisory notes go into the {@code diagnostics} string list on success.
 *
 * <p>Filesystem-free: no reads or writes. Source text arrives via params,
 * output goes via JSON response. The Rust client owns file I/O.
 */
final class EmitTypeHandler {

    private final JavaParser parser;

    EmitTypeHandler() {
        this.parser = JavaParser.fromJavaVersion()
                .build();
    }

    /**
     * Validate the macro-supplied source text and return the file-create descriptor.
     *
     * <p>The content of the created file comes from {@code params.getSourceText()},
     * not from a skeleton generator. OpenRewrite parses the supplied source to confirm
     * it is a valid Java compilation unit, then verifies that the declared package,
     * type name, and kind match the request params. {@code printAll()} is used for a
     * format-preserving round-trip that proves validity.
     *
     * @param params validated emit-type parameters
     * @return result containing file_creates and an empty diagnostics list
     * @throws SidecarException with {@link SidecarException#PARSE_INVALID} when
     *     source_text does not parse as a valid Java compilation unit, or with
     *     {@link SidecarException#TYPE_MISMATCH} when the declared package, name,
     *     or kind in source_text differs from the request params
     */
    EmitTypeResult handle(EmitTypeParams params) {
        // -- validate kind -------------------------------------------------
        String kind = params.getKind().toLowerCase();
        if (!isValidKind(kind)) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Invalid kind '" + params.getKind()
                    + "'. Supported: class, interface, enum, record");
        }

        // -- validate name -------------------------------------------------
        String name = params.getName();
        if (!isValidJavaIdentifier(name)) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Invalid type name '" + name
                    + "'. Must be a valid Java identifier");
        }

        // -- require source_text -------------------------------------------
        String sourceText = params.getSourceText();
        if (sourceText == null || sourceText.isBlank()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: source_text is required and must not be blank");
        }

        // -- parse + validate with OpenRewrite -----------------------------
        // parser.reset() is called inside parseOne() before every parse() call
        // (JavaParser is stateful and must be reset between batches).
        InMemoryExecutionContext ctx = new InMemoryExecutionContext(
                t -> { /* suppress stderr logging */ });

        J.CompilationUnit cu;
        try {
            Optional<SourceFile> opt = parseOne(sourceText, ctx);
            if (opt.isEmpty()) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: Parser produced empty result");
            }
            SourceFile sf = opt.get();
            if (!(sf instanceof J.CompilationUnit compilationUnit)) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: source_text did not parse as a valid Java compilation unit");
            }
            cu = compilationUnit;
        } catch (SidecarException e) {
            throw e;
        } catch (Exception e) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: OpenRewrite parse failure: " + e.getMessage());
        }

        // -- guard: compilation unit must contain at least one type declaration.
        // When source_text is syntactically malformed (e.g. "package x; this is not java"),
        // OpenRewrite may return a J.CompilationUnit with an empty classes list rather than
        // a ParseError node. Treat no-class CUs as a parse failure (PARSE_INVALID) so the
        // caller gets the right error code — not TYPE_MISMATCH from the J5 lookup below.
        if (cu.getClasses().isEmpty()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: source_text contains no type declarations "
                    + "(the source may be syntactically invalid)");
        }

        // -- J5: verify package declaration --------------------------------
        String parsedPackage = cu.getPackageDeclaration() != null
                ? cu.getPackageDeclaration().getPackageName()
                : "";
        String expectedPackage = params.getPackageName() != null
                ? params.getPackageName()
                : "";
        if (!parsedPackage.equals(expectedPackage)) {
            throw new SidecarException(SidecarException.TYPE_MISMATCH,
                    "error.type_mismatch: source_text declares package '" + parsedPackage
                    + "' but params.package is '" + expectedPackage + "'");
        }

        // -- J5: verify top-level type name and kind -----------------------
        boolean typeFound = false;
        for (J.ClassDeclaration cd : cu.getClasses()) {
            if (cd.getSimpleName().equals(name)) {
                String actualKind = classKind(cd);
                if (!actualKind.equals(kind)) {
                    throw new SidecarException(SidecarException.TYPE_MISMATCH,
                            "error.type_mismatch: source_text declares '" + name
                            + "' as " + actualKind + " but params.kind is '" + kind + "'");
                }
                typeFound = true;
                break;
            }
        }
        if (!typeFound) {
            throw new SidecarException(SidecarException.TYPE_MISMATCH,
                    "error.type_mismatch: source_text does not contain a top-level type named '"
                    + name + "'");
        }

        // -- format via OpenRewrite print (format-preserving round-trip) ---
        String formattedSource = cu.printAll();

        // -- build output path ---------------------------------------------
        String filePath = buildFilePath(params.getSourceRoot(),
                params.getPackageName(), name);

        return new EmitTypeResult(
                List.of(new FileCreate(filePath, formattedSource)),
                List.of());
    }

    // -----------------------------------------------------------------------
    // Parse helper — reset() before every parse() call (JavaParser is stateful)
    // -----------------------------------------------------------------------

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    // -----------------------------------------------------------------------
    // Kind classification from OpenRewrite AST
    // -----------------------------------------------------------------------

    /**
     * Returns the kind string for a class declaration, matching the wire protocol
     * values: {@code class}, {@code interface}, {@code enum}, {@code record},
     * {@code annotation}.
     */
    private static String classKind(J.ClassDeclaration cd) {
        // In OpenRewrite 8.x, J.ClassDeclaration.getKind() returns the
        // J.ClassDeclaration.Kind.Type enum directly — no further .getType() needed.
        return switch (cd.getKind()) {
            case Class -> "class";
            case Interface -> "interface";
            case Enum -> "enum";
            case Record -> "record";
            case Annotation -> "annotation";
            // OpenRewrite's Kind.Type carries additional constants (e.g. Value)
            // that are not emit targets; fall back to the lowercased enum name.
            default -> cd.getKind().name().toLowerCase();
        };
    }

    // -----------------------------------------------------------------------
    // Path construction
    // -----------------------------------------------------------------------

    private static String buildFilePath(String sourceRoot, String packageName, String name) {
        String normalizedRoot = sourceRoot;
        if (normalizedRoot.endsWith("/")) {
            normalizedRoot = normalizedRoot.substring(0, normalizedRoot.length() - 1);
        }

        String packagePath = packageName != null && !packageName.isEmpty()
                ? packageName.replace('.', '/')
                : "";

        if (packagePath.isEmpty()) {
            return normalizedRoot + "/" + name + ".java";
        }
        return normalizedRoot + "/" + packagePath + "/" + name + ".java";
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    private static boolean isValidKind(String kind) {
        return "class".equals(kind)
                || "interface".equals(kind)
                || "enum".equals(kind)
                || "record".equals(kind);
    }

    private static boolean isValidJavaIdentifier(String name) {
        if (name == null || name.isEmpty()) {
            return false;
        }
        if (!Character.isJavaIdentifierStart(name.charAt(0))) {
            return false;
        }
        for (int i = 1; i < name.length(); i++) {
            if (!Character.isJavaIdentifierPart(name.charAt(i))) {
                return false;
            }
        }
        return true;
    }
}
