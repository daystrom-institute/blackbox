package com.blackbox.javaworker;

import java.util.List;
import java.util.Optional;
import java.util.stream.Collectors;
import org.openrewrite.Cursor;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.tree.J;
import org.openrewrite.java.tree.Statement;
import org.openrewrite.java.tree.TypeTree;

/**
 * Handles the {@code replaceMethodBody} JSON-RPC method.
 *
 * <p>Finds a method by name and parameter types in the target type, then replaces
 * its body with the caller-supplied {@code new_body} text. The method is located via
 * OpenRewrite's LST; the replacement is applied via a {@link JavaIsoVisitor}.
 *
 * <h3>Idempotency</h3>
 * <p>If the existing method body already equals the new body (format-preserving round-trip
 * via {@code printTrimmed}), the handler returns a no-op result rather than re-writing.
 *
 * <h3>Semantic failures</h3>
 * <ul>
 *   <li>{@link SidecarException#PARSE_INVALID} — source_text or new_body failed to parse.</li>
 *   <li>{@link SidecarException#METHOD_NOT_FOUND} — no method matching name + parameter_types.</li>
 *   <li>{@link SidecarException#METHOD_AMBIGUOUS} — multiple methods match.</li>
 * </ul>
 *
 * <p>Filesystem-free: all source content arrives via params; all output goes via JSON
 * response. The Rust client owns file I/O and transactions.
 */
final class ReplaceMethodBodyHandler {

    private final JavaParser parser;

    ReplaceMethodBodyHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    /**
     * Replace the body of the named method in the target type.
     *
     * @param params validated replace-method-body parameters
     * @return result with rewritten source, change flags, and diagnostics
     * @throws SidecarException on semantic failure (parse error, method not found, ambiguous)
     */
    ReplaceMethodBodyResult handle(ReplaceMethodBodyParams params) {
        String sourceText  = params.getSourceText();
        String targetType  = params.getTargetType();
        String methodName  = params.getMethodName();
        List<String> parameterTypes = params.getParameterTypes(); // never null (Protocol default)
        String newBody     = params.getNewBody();
        List<String> imports = params.getImports();               // never null (Protocol default)

        InMemoryExecutionContext ctx = new InMemoryExecutionContext(t -> { /* suppress stderr */ });

        // -- (1) Parse existing source ------------------------------------
        J.CompilationUnit cu = parseCompilationUnit(sourceText, ctx);

        // -- (2) Find target type -----------------------------------------
        J.ClassDeclaration targetCd = findClass(cu, targetType);
        if (targetCd == null) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Target type '" + targetType + "' not found in source");
        }

        // -- (3) Find method by name + parameter_types --------------------
        List<J.MethodDeclaration> byName = findMethodsByName(targetCd, methodName);
        if (byName.isEmpty()) {
            throw new SidecarException(SidecarException.METHOD_NOT_FOUND,
                    "error.method_not_found: No method named '" + methodName
                    + "' found in type '" + targetType + "'");
        }
        List<J.MethodDeclaration> matching = filterByParamTypes(byName, parameterTypes, cu);
        if (matching.isEmpty()) {
            throw new SidecarException(SidecarException.METHOD_NOT_FOUND,
                    "error.method_not_found: No method '" + methodName
                    + "' with the specified parameter types found in '" + targetType + "'");
        }
        if (matching.size() > 1) {
            throw new SidecarException(SidecarException.METHOD_AMBIGUOUS,
                    "error.method_ambiguous: Multiple methods named '" + methodName
                    + "' match the given parameter types in '" + targetType
                    + "' — add parameter_types to disambiguate");
        }
        J.MethodDeclaration target = matching.get(0);

        // -- (4) Parse new body in a wrapper class ------------------------
        // Wrap new_body in a synthetic method so the parser can produce a J.Block.
        String wrapperSrc = "class __W__ { void __m__() {\n" + newBody + "\n} }";
        J.CompilationUnit wrapperCu = parseCompilationUnit(wrapperSrc, ctx);
        J.Block newBlock = extractWrapperMethodBlock(wrapperCu);

        // -- (5) Idempotency: compare existing body to new body ----------
        J.Block existingBlock = target.getBody();
        if (existingBlock != null) {
            String existingText = existingBlock
                    .printTrimmed(new Cursor(new Cursor(null, cu), existingBlock))
                    .strip();
            String newText = newBlock
                    .printTrimmed(new Cursor(new Cursor(null, wrapperCu), newBlock))
                    .strip();
            if (existingText.equals(newText)) {
                return ReplaceMethodBodyResult.noOp(sourceText,
                        "Method '" + methodName + "' body already matches new_body");
            }
        }

        // -- (6) Replace body via LST visitor ----------------------------
        J.CompilationUnit modified = (J.CompilationUnit) new BodyReplacer(
                targetType, methodName, parameterTypes, newBlock, imports).visit(cu, ctx);

        // -- (7) Format and return ----------------------------------------
        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? ReplaceMethodBodyResult.changed(rewrittenSource)
                : ReplaceMethodBodyResult.noOp(sourceText, "No changes after formatting");
    }

    // -----------------------------------------------------------------------
    // Parse helpers — reset() before every parse() call (JavaParser is stateful)
    // -----------------------------------------------------------------------

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    private J.CompilationUnit parseCompilationUnit(String src, InMemoryExecutionContext ctx) {
        try {
            Optional<SourceFile> opt = parseOne(src, ctx);
            if (opt.isEmpty()) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: Parser produced empty result");
            }
            if (!(opt.get() instanceof J.CompilationUnit cu)) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: Parsed source is not a CompilationUnit");
            }
            return cu;
        } catch (SidecarException e) {
            throw e;
        } catch (Exception e) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Parse error: " + e.getMessage());
        }
    }

    /** Extract the J.Block from the first method in a wrapper class. */
    private static J.Block extractWrapperMethodBlock(J.CompilationUnit wrapperCu) {
        if (wrapperCu.getClasses().isEmpty()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Wrapper class produced no declarations");
        }
        J.ClassDeclaration wClass = wrapperCu.getClasses().get(0);
        for (Statement stmt : wClass.getBody().getStatements()) {
            if (stmt instanceof J.MethodDeclaration wMethod) {
                J.Block block = wMethod.getBody();
                if (block == null) {
                    throw new SidecarException(SidecarException.PARSE_INVALID,
                            "error.parse_invalid: Wrapper method has no body — new_body may be invalid");
                }
                return block;
            }
        }
        throw new SidecarException(SidecarException.PARSE_INVALID,
                "error.parse_invalid: Could not extract a method body from new_body wrapper");
    }

    // -----------------------------------------------------------------------
    // Method-finding helpers
    // -----------------------------------------------------------------------

    /** Find the class declaration with the given simple name in a compilation unit. */
    private static J.ClassDeclaration findClass(J.CompilationUnit cu, String name) {
        for (J.ClassDeclaration cd : cu.getClasses()) {
            if (cd.getSimpleName().equals(name)) {
                return cd;
            }
        }
        return null;
    }

    /** Return all J.MethodDeclaration members with the given simple name in a class. */
    private static List<J.MethodDeclaration> findMethodsByName(
            J.ClassDeclaration cd, String name) {
        if (cd.getBody() == null) {
            return List.of();
        }
        return cd.getBody().getStatements().stream()
                .filter(s -> s instanceof J.MethodDeclaration md
                        && md.getSimpleName().equals(name))
                .map(s -> (J.MethodDeclaration) s)
                .collect(Collectors.toList());
    }

    /**
     * Filter method declarations to those whose parameter types match {@code expectedTypes}.
     *
     * <p>Type names are compared by their written form (via {@code printTrimmed}), so
     * {@code "String"} and {@code "java.lang.String"} are NOT equal — use whatever form
     * the source declares.
     */
    private static List<J.MethodDeclaration> filterByParamTypes(
            List<J.MethodDeclaration> candidates,
            List<String> expectedTypes,
            J.CompilationUnit cu) {
        return candidates.stream()
                .filter(m -> paramTypesMatch(m, expectedTypes, cu))
                .collect(Collectors.toList());
    }

    private static boolean paramTypesMatch(
            J.MethodDeclaration method,
            List<String> expectedTypes,
            J.CompilationUnit cu) {
        List<Statement> params = method.getParameters().stream()
                .filter(p -> !(p instanceof J.Empty))
                .collect(Collectors.toList());
        if (params.size() != expectedTypes.size()) {
            return false;
        }
        for (int i = 0; i < params.size(); i++) {
            if (!(params.get(i) instanceof J.VariableDeclarations vd)) {
                return false;
            }
            TypeTree typeExpr = vd.getTypeExpression();
            if (typeExpr == null) {
                return false;
            }
            String actual = typeExpr
                    .printTrimmed(new Cursor(new Cursor(null, cu), typeExpr))
                    .strip();
            if (!actual.equals(expectedTypes.get(i).strip())) {
                return false;
            }
        }
        return true;
    }

    // -----------------------------------------------------------------------
    // Visitor: replace the method body
    // -----------------------------------------------------------------------

    /**
     * Visitor that replaces the body of a named method inside {@code targetType}.
     *
     * <p>Matches the first unvisited method whose simple name and parameter types agree
     * with the original match (uniqueness was verified before calling this visitor).
     * Uses the {@code replaced} flag to stop after the first replacement.
     */
    private static final class BodyReplacer
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetType;
        private final String methodName;
        private final List<String> expectedParamTypes;
        private final J.Block newBlock;
        private final List<String> imports;
        private boolean replaced = false;

        BodyReplacer(
                String targetType,
                String methodName,
                List<String> expectedParamTypes,
                J.Block newBlock,
                List<String> imports) {
            this.targetType = targetType;
            this.methodName = methodName;
            this.expectedParamTypes = expectedParamTypes;
            this.newBlock = newBlock;
            this.imports = imports;
        }

        @Override
        public J.MethodDeclaration visitMethodDeclaration(
                J.MethodDeclaration method,
                InMemoryExecutionContext ctx) {

            if (!replaced && method.getSimpleName().equals(methodName)) {
                // Guard: only operate inside the target class, not a nested one with the same name.
                J.ClassDeclaration enclosingClass =
                        getCursor().firstEnclosing(J.ClassDeclaration.class);
                if (enclosingClass != null
                        && enclosingClass.getSimpleName().equals(targetType)) {
                    // Use the live enclosing CU from the visitor cursor for printTrimmed.
                    J.CompilationUnit enclosingCu =
                            getCursor().firstEnclosing(J.CompilationUnit.class);
                    if (enclosingCu != null
                            && paramTypesMatch(method, expectedParamTypes, enclosingCu)) {
                        for (String fqn : imports) {
                            maybeAddImport(fqn);
                        }
                        replaced = true;
                        return method.withBody(newBlock);
                    }
                }
            }
            return super.visitMethodDeclaration(method, ctx);
        }

        private static boolean paramTypesMatch(
                J.MethodDeclaration method,
                List<String> expectedTypes,
                J.CompilationUnit cu) {
            List<Statement> params = method.getParameters().stream()
                    .filter(p -> !(p instanceof J.Empty))
                    .collect(Collectors.toList());
            if (params.size() != expectedTypes.size()) {
                return false;
            }
            for (int i = 0; i < params.size(); i++) {
                if (!(params.get(i) instanceof J.VariableDeclarations vd)) {
                    return false;
                }
                TypeTree typeExpr = vd.getTypeExpression();
                if (typeExpr == null) {
                    return false;
                }
                String actual = typeExpr
                        .printTrimmed(new Cursor(new Cursor(null, cu), typeExpr))
                        .strip();
                if (!actual.equals(expectedTypes.get(i).strip())) {
                    return false;
                }
            }
            return true;
        }
    }
}
