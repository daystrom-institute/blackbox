package com.blackbox.javaworker;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.stream.Collectors;
import org.openrewrite.Cursor;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.tree.J;
import org.openrewrite.java.tree.Statement;

/**
 * Handles the {@code insertMember} JSON-RPC method.
 *
 * <p>Inserts a member declaration (field, method, constructor, inner type,
 * annotation, or initializer) into an existing Java type. Uses OpenRewrite's
 * LST to find the target type, validate the result, and produce formatted output.
 *
 * <h3>Idempotency and conflict rules (J4)</h3>
 * <ul>
 *   <li>If the member text matches the existing member exactly (after
 *       format-preserving print) → {@link InsertMemberResult#noOp} (idempotent).</li>
 *   <li>For {@code J.MethodDeclaration}: same simpleName + same parameter count,
 *       but different body → {@link SidecarException#MEMBER_CONFLICT}.</li>
 *   <li>For {@code J.VariableDeclarations} or {@code J.ClassDeclaration}: same
 *       name, different text → {@link SidecarException#MEMBER_CONFLICT}.</li>
 *   <li>Same method name but different parameter count → overload, proceed with
 *       insertion.</li>
 * </ul>
 *
 * <p>Semantic failures are reported as {@link SidecarException}, which the
 * {@link Dispatcher} converts to a JSON-RPC error response.
 *
 * <p>Filesystem-free: all source content arrives via params; all output goes
 * via JSON response. The Rust client owns file I/O and transactions.
 */
final class InsertMemberHandler {

    private final JavaParser parser;

    InsertMemberHandler() {
        this.parser = JavaParser.fromJavaVersion()
                .build();
    }

    /**
     * Insert a member into the target type within the given source text.
     *
     * @param params validated insert-member parameters
     * @return result with rewritten source, change flags, and diagnostics
     * @throws SidecarException with {@link SidecarException#PARSE_INVALID} when
     *     source_text or member_text does not parse, or with
     *     {@link SidecarException#MEMBER_CONFLICT} when a same-signature member
     *     already exists with a different body
     */
    InsertMemberResult handle(InsertMemberParams params) {
        String sourceText = params.getSourceText();
        String targetType = params.getTargetType();
        String memberText = params.getMemberText();
        List<String> imports = params.getImports() != null
                ? params.getImports()
                : List.of();

        InMemoryExecutionContext ctx = new InMemoryExecutionContext(
                t -> { /* suppress stderr */ });

        // -- (1) Parse existing source ------------------------------------
        // parseOne() calls parser.reset() before every parse() invocation;
        // JavaParser is stateful and must be reset between batches of source files.
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
                        "error.parse_invalid: Parsed source is not a CompilationUnit");
            }
            cu = compilationUnit;
        } catch (SidecarException e) {
            throw e;
        } catch (Exception e) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Parse error: " + e.getMessage());
        }

        // -- (2) Find target type declaration -----------------------------
        TargetFinder finder = new TargetFinder(targetType);
        finder.visit(cu, ctx);
        J.ClassDeclaration targetCd = finder.getFound();
        if (targetCd == null) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Target type '" + targetType + "' not found in source");
        }

        // -- (3) Parse member text in a wrapper class ---------------------
        // Parsing happens before the idempotency check so we can compare
        // parsed AST nodes (not raw strings) for conflict detection.
        // wrapperCu is declared here so step 4 can use it for cursor construction.
        J.CompilationUnit wrapperCu;
        List<Statement> memberStatements;
        try {
            String wrapper = "class __InsertMemberWrapper__ {\n" + memberText + "\n}";
            Optional<SourceFile> opt = parseOne(wrapper, ctx);
            if (opt.isEmpty()) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: Failed to parse member text");
            }
            wrapperCu = (J.CompilationUnit) opt.get();
            if (wrapperCu.getClasses().isEmpty()) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: Member text did not produce a valid declaration");
            }
            J.ClassDeclaration wrapperClass = wrapperCu.getClasses().get(0);
            memberStatements = new ArrayList<>(wrapperClass.getBody().getStatements());
        } catch (SidecarException e) {
            throw e;
        } catch (Exception e) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Failed to parse member text: " + e.getMessage());
        }

        // -- (4) Check idempotency / conflict for each new member ---------
        // J4: Distinguish idempotent no_op (same text) from conflict
        // (same signature, different body).
        //
        // Cursor idiom for printing a sub-node outside a visitor pass:
        //   node.printTrimmed(new Cursor(new Cursor(null, enclosingCu), node))
        // The parent cursor holds the enclosing J.CompilationUnit (a JavaSourceFile),
        // so JavaPrinter.firstEnclosing(JavaSourceFile) succeeds without crashing.
        // Existing members are printed with `cu` (the target CU);
        // new members are printed with `wrapperCu` (the wrapper CU they came from).
        for (Statement newStmt : memberStatements) {
            String newName = nodeName(newStmt);
            if (newName == null) {
                continue; // anonymous / unnamed — can't classify; proceed
            }
            Statement existingStmt = findMemberByName(targetCd, newName);
            if (existingStmt == null) {
                continue; // no member with this name — no conflict
            }

            if (newStmt instanceof J.MethodDeclaration newMethod
                    && existingStmt instanceof J.MethodDeclaration existingMethod) {
                // For methods: different param count → overload, not a conflict.
                int newParamCount = countParams(newMethod);
                int existingParamCount = countParams(existingMethod);
                if (newParamCount != existingParamCount) {
                    continue; // overloaded method — proceed to insert
                }
                // Same name + same param count: compare printed text for idempotency.
                String existingText = existingMethod
                        .printTrimmed(new Cursor(new Cursor(null, cu), existingMethod))
                        .strip();
                String newText = newMethod
                        .printTrimmed(new Cursor(new Cursor(null, wrapperCu), newMethod))
                        .strip();
                if (existingText.equals(newText)) {
                    return InsertMemberResult.noOp(sourceText,
                            "Member '" + newName + "' already exists in " + targetType);
                } else {
                    throw new SidecarException(SidecarException.MEMBER_CONFLICT,
                            "error.member_conflict: method '" + newName
                            + "' with same signature already exists in " + targetType
                            + " with a different body");
                }
            } else {
                // Field or inner type: name match alone is sufficient for comparison.
                String existingText = existingStmt
                        .printTrimmed(new Cursor(new Cursor(null, cu), existingStmt))
                        .strip();
                String newText = newStmt
                        .printTrimmed(new Cursor(new Cursor(null, wrapperCu), newStmt))
                        .strip();
                if (existingText.equals(newText)) {
                    return InsertMemberResult.noOp(sourceText,
                            "Member '" + newName + "' already exists in " + targetType);
                } else {
                    throw new SidecarException(SidecarException.MEMBER_CONFLICT,
                            "error.member_conflict: member '" + newName
                            + "' already exists in " + targetType
                            + " with a different definition");
                }
            }
        }

        // -- (5) Insert members and queue imports via LST visitor ---------
        // JavaVisitor.visit() returns @Nullable J; cast is safe because we
        // visit a J.CompilationUnit and our visitor returns the same type.
        // Imports are queued via maybeAddImport() inside MemberInserter and
        // materialised by OpenRewrite during the same visitor pass.
        J.CompilationUnit modified = (J.CompilationUnit) new MemberInserter(
                targetType, memberStatements, imports).visit(cu, ctx);

        // -- (6) Format and return ----------------------------------------
        String rewrittenSource = modified.printAll();

        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? InsertMemberResult.changed(rewrittenSource)
                : InsertMemberResult.noOp(sourceText, "No changes after formatting");
    }

    // -----------------------------------------------------------------------
    // Parse helper — reset() before every parse() call (JavaParser is stateful)
    // -----------------------------------------------------------------------

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    // -----------------------------------------------------------------------
    // Conflict detection helpers
    // -----------------------------------------------------------------------

    /**
     * Find the first member in {@code cd} whose simple name equals {@code name}.
     * Returns {@code null} if no such member exists.
     */
    private static Statement findMemberByName(J.ClassDeclaration cd, String name) {
        if (cd.getBody() == null) {
            return null;
        }
        for (Statement stmt : cd.getBody().getStatements()) {
            if (Objects.equals(nodeName(stmt), name)) {
                return stmt;
            }
        }
        return null;
    }

    /**
     * Count the non-empty parameters of a method declaration.
     *
     * <p>OpenRewrite represents no-arg methods as a list containing a single
     * {@link J.Empty} element; those are excluded from the count.
     */
    private static int countParams(J.MethodDeclaration method) {
        return (int) method.getParameters().stream()
                .filter(p -> !(p instanceof J.Empty))
                .count();
    }

    /** Extract the declared name from a statement (method, field, inner type). */
    private static String nodeName(Statement stmt) {
        if (stmt instanceof J.MethodDeclaration md) {
            return md.getSimpleName();
        }
        if (stmt instanceof J.VariableDeclarations vd) {
            if (!vd.getVariables().isEmpty()) {
                return vd.getVariables().get(0).getSimpleName();
            }
        }
        if (stmt instanceof J.ClassDeclaration cd) {
            return cd.getSimpleName();
        }
        return null;
    }

    // -----------------------------------------------------------------------
    // Visitors
    // -----------------------------------------------------------------------

    /** Visitor that finds a class declaration by simple name. */
    private static final class TargetFinder extends JavaIsoVisitor<InMemoryExecutionContext> {
        private final String targetName;
        private J.ClassDeclaration found;

        TargetFinder(String targetName) {
            this.targetName = targetName;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {
            if (cd.getSimpleName().equals(targetName)) {
                found = cd;
            }
            return super.visitClassDeclaration(cd, ctx);
        }

        J.ClassDeclaration getFound() { return found; }
    }

    /**
     * Visitor that inserts member statements into the end of a target class body
     * and queues import additions via {@link #maybeAddImport(String)}.
     *
     * <p>OpenRewrite's {@code maybeAddImport} handles deduplication: if the import
     * is already present it is left untouched; new imports are inserted with correct
     * layout during the same visitor pass.
     */
    private static final class MemberInserter
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetName;
        private final List<Statement> memberStatements;
        private final List<String> imports;

        MemberInserter(
                String targetName,
                List<Statement> memberStatements,
                List<String> imports) {
            this.targetName = targetName;
            this.memberStatements = memberStatements;
            this.imports = imports;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {

            if (!cd.getSimpleName().equals(targetName)) {
                return super.visitClassDeclaration(cd, ctx);
            }

            J.Block body = cd.getBody();
            if (body == null) {
                return super.visitClassDeclaration(cd, ctx);
            }

            // Keep existing statements, append new ones.
            List<Statement> all = new ArrayList<>(body.getStatements());
            all.addAll(memberStatements);

            // Queue import additions via OpenRewrite's import API.
            // maybeAddImport deduplicates: already-present imports are left alone.
            for (String fqn : imports) {
                maybeAddImport(fqn);
            }

            J.Block newBody = body.withStatements(all);
            J.ClassDeclaration updated = cd.withBody(newBody);
            return super.visitClassDeclaration(updated, ctx);
        }
    }
}
