package com.blackbox.javaworker;

import java.util.ArrayList;
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
 * Handles the {@code insertStatementInMethod} JSON-RPC method.
 *
 * <p>Finds a method by name and parameter types in the target type, then appends
 * one or more statements to its body. The statement text is parsed in a wrapper class
 * so OpenRewrite can produce typed LST nodes; the result is applied via a
 * {@link JavaIsoVisitor}.
 *
 * <h3>v1 Placement</h3>
 * <p>Only {@code "append"} is supported in v1. Any other value is rejected with
 * {@link SidecarException#PARSE_INVALID}.
 *
 * <h3>Idempotency</h3>
 * <p>If any of the parsed new statements already appears in the method body
 * (by format-preserving {@code printTrimmed} comparison), the handler returns a
 * no-op result rather than producing a duplicate.
 *
 * <h3>Semantic failures</h3>
 * <ul>
 *   <li>{@link SidecarException#PARSE_INVALID} — source_text, statement_text, or
 *       placement is invalid.</li>
 *   <li>{@link SidecarException#METHOD_NOT_FOUND} — no method matching name + parameter_types.</li>
 *   <li>{@link SidecarException#METHOD_AMBIGUOUS} — multiple methods match.</li>
 * </ul>
 *
 * <p>Filesystem-free: all source content arrives via params; all output goes via JSON
 * response. The Rust client owns file I/O and transactions.
 */
final class InsertStatementInMethodHandler {

    private final JavaParser parser;

    InsertStatementInMethodHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    /**
     * Append statement(s) to the named method body in the target type.
     *
     * @param params validated insert-statement-in-method parameters
     * @return result with rewritten source, change flags, and diagnostics
     * @throws SidecarException on semantic failure (parse error, method not found, ambiguous)
     */
    InsertStatementInMethodResult handle(InsertStatementInMethodParams params) {
        String sourceText    = params.getSourceText();
        String targetType    = params.getTargetType();
        String methodName    = params.getMethodName();
        List<String> parameterTypes = params.getParameterTypes(); // never null
        String statementText = params.getStatementText();
        String placement     = params.getPlacement();             // default "append"
        List<String> imports = params.getImports();               // never null

        // -- validate placement (v1: append only) -------------------------
        if (!"append".equals(placement)) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Unsupported placement '" + placement
                    + "'. Only \"append\" is supported in v1.");
        }

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

        // -- (4) Parse statement_text in a wrapper class ------------------
        // The wrapper gives OpenRewrite enough context to produce typed Statement nodes.
        String wrapperSrc = "class __W__ { void __m__() {\n" + statementText + "\n} }";
        J.CompilationUnit wrapperCu = parseCompilationUnit(wrapperSrc, ctx);
        List<Statement> newStatements = extractWrapperBodyStatements(wrapperCu);
        if (newStatements.isEmpty()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: statement_text produced no statements");
        }

        // -- (5) Idempotency: check if any new statement already exists --
        // Collect printTrimmed strings for all existing body statements.
        J.Block existingBody = target.getBody();
        if (existingBody != null && !existingBody.getStatements().isEmpty()) {
            List<String> existingPrinted = existingBody.getStatements().stream()
                    .map(s -> s.printTrimmed(new Cursor(new Cursor(null, cu), s)).strip())
                    .collect(Collectors.toList());

            // Collect printTrimmed for all new statements.
            List<String> newPrinted = newStatements.stream()
                    .map(s -> s.printTrimmed(new Cursor(new Cursor(null, wrapperCu), s)).strip())
                    .collect(Collectors.toList());

            // If ALL new statements are already in the body → no-op.
            boolean allPresent = newPrinted.stream()
                    .allMatch(existingPrinted::contains);
            if (allPresent) {
                return InsertStatementInMethodResult.noOp(sourceText,
                        "All statement(s) already present in method '" + methodName + "'");
            }
        }

        // -- (6) Append statements via LST visitor ------------------------
        J.CompilationUnit modified = (J.CompilationUnit) new StatementAppender(
                targetType, methodName, parameterTypes, newStatements, imports)
                .visit(cu, ctx);

        // -- (7) Format and return ----------------------------------------
        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? InsertStatementInMethodResult.changed(rewrittenSource)
                : InsertStatementInMethodResult.noOp(sourceText, "No changes after formatting");
    }

    // -----------------------------------------------------------------------
    // Parse helpers
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

    /** Extract all body statements from the first method in a wrapper class. */
    private static List<Statement> extractWrapperBodyStatements(J.CompilationUnit wrapperCu) {
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
                            "error.parse_invalid: Wrapper method has no body");
                }
                return new ArrayList<>(block.getStatements());
            }
        }
        throw new SidecarException(SidecarException.PARSE_INVALID,
                "error.parse_invalid: Could not extract statements from statement_text wrapper");
    }

    // -----------------------------------------------------------------------
    // Method-finding helpers
    // -----------------------------------------------------------------------

    private static J.ClassDeclaration findClass(J.CompilationUnit cu, String name) {
        for (J.ClassDeclaration cd : cu.getClasses()) {
            if (cd.getSimpleName().equals(name)) {
                return cd;
            }
        }
        return null;
    }

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
    // Visitor: append statements to the method body
    // -----------------------------------------------------------------------

    /**
     * Visitor that appends statements to the body of a named method inside
     * {@code targetType}. Queues import additions via {@link #maybeAddImport(String)}.
     *
     * <p>Matches the first unvisited method satisfying the name + parameter-type
     * constraint; the {@code appended} flag prevents double-insertion.
     */
    private static final class StatementAppender
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetType;
        private final String methodName;
        private final List<String> expectedParamTypes;
        private final List<Statement> newStatements;
        private final List<String> imports;
        private boolean appended = false;

        StatementAppender(
                String targetType,
                String methodName,
                List<String> expectedParamTypes,
                List<Statement> newStatements,
                List<String> imports) {
            this.targetType = targetType;
            this.methodName = methodName;
            this.expectedParamTypes = expectedParamTypes;
            this.newStatements = newStatements;
            this.imports = imports;
        }

        @Override
        public J.MethodDeclaration visitMethodDeclaration(
                J.MethodDeclaration method,
                InMemoryExecutionContext ctx) {

            if (!appended && method.getSimpleName().equals(methodName)) {
                J.ClassDeclaration enclosingClass =
                        getCursor().firstEnclosing(J.ClassDeclaration.class);
                if (enclosingClass != null
                        && enclosingClass.getSimpleName().equals(targetType)) {
                    J.CompilationUnit enclosingCu =
                            getCursor().firstEnclosing(J.CompilationUnit.class);
                    if (enclosingCu != null
                            && paramTypesMatch(method, expectedParamTypes, enclosingCu)) {
                        J.Block body = method.getBody();
                        if (body != null) {
                            List<Statement> all = new ArrayList<>(body.getStatements());
                            all.addAll(newStatements);
                            for (String fqn : imports) {
                                maybeAddImport(fqn);
                            }
                            appended = true;
                            return method.withBody(body.withStatements(all));
                        }
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
