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
 * Handles the {@code deleteMember} JSON-RPC method.
 *
 * <p>Removes a member (method, constructor, or field) from a type by simple
 * name, optionally disambiguated by written parameter types. Generic by
 * construction: no library-specific knowledge. Composed with the macro layer's
 * {@code ForEach} to sweep away hand-written boilerplate that a class-level
 * annotation now generates (e.g. delete every trivial getter once {@code
 * @Getter} is added).
 *
 * <h3>Matching rules</h3>
 * <ul>
 *   <li>Candidates are class-body members whose simple name equals
 *       {@code member_name}: methods, constructors (whose simple name is the
 *       type name), and fields.</li>
 *   <li>When {@code parameter_types} is supplied, only methods/constructors
 *       whose written parameter types match (by printed form, in order) are
 *       candidates — fields are excluded.</li>
 *   <li>Zero candidates → {@link DeleteMemberResult#noOp} (idempotent).</li>
 *   <li>{@code parameter_types == null} and more than one candidate →
 *       {@link SidecarException#MEMBER_AMBIGUOUS} (fail closed; add
 *       parameter_types to disambiguate).</li>
 * </ul>
 *
 * <p>Filesystem-free: all source content arrives via params; output goes via
 * JSON. The Rust client owns file I/O and transactions.
 */
final class DeleteMemberHandler {

    private final JavaParser parser;

    DeleteMemberHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    DeleteMemberResult handle(DeleteMemberParams params) {
        String sourceText = params.getSourceText();
        String targetType = params.getTargetType();
        String memberName = params.getMemberName();
        // null = match by name alone; non-null (incl. empty) = method/ctor with
        // exactly these written parameter types.
        List<String> paramTypes = params.getParameterTypes();

        InMemoryExecutionContext ctx = new InMemoryExecutionContext(t -> { /* suppress */ });

        // -- (1) Parse source ---------------------------------------------
        J.CompilationUnit cu;
        try {
            Optional<SourceFile> opt = parseOne(sourceText, ctx);
            if (opt.isEmpty() || !(opt.get() instanceof J.CompilationUnit compilationUnit)) {
                throw new SidecarException(SidecarException.PARSE_INVALID,
                        "error.parse_invalid: source did not parse to a CompilationUnit");
            }
            cu = compilationUnit;
        } catch (SidecarException e) {
            throw e;
        } catch (Exception e) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Parse error: " + e.getMessage());
        }

        // -- (2) Find target type -----------------------------------------
        TargetFinder finder = new TargetFinder(targetType);
        finder.visit(cu, ctx);
        J.ClassDeclaration targetCd = finder.getFound();
        if (targetCd == null) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Target type '" + targetType + "' not found in source");
        }
        if (targetCd.getBody() == null) {
            return DeleteMemberResult.noOp(sourceText, "Target type has no body");
        }

        // -- (3) Collect candidates by name (+ optional param types) ------
        List<Statement> candidates = new ArrayList<>();
        for (Statement stmt : targetCd.getBody().getStatements()) {
            if (memberMatches(stmt, memberName, paramTypes, cu)) {
                candidates.add(stmt);
            }
        }

        if (candidates.isEmpty()) {
            return DeleteMemberResult.noOp(sourceText,
                    "No member named '" + memberName + "' in " + targetType);
        }
        if (paramTypes == null && candidates.size() > 1) {
            throw new SidecarException(SidecarException.MEMBER_AMBIGUOUS,
                    "error.member_ambiguous: member '" + memberName + "' in " + targetType
                    + " matches " + candidates.size()
                    + " declarations — supply parameter_types to disambiguate");
        }

        // -- (4) Remove the matched member(s) -----------------------------
        J.CompilationUnit modified = (J.CompilationUnit) new MemberRemover(
                targetType, memberName, paramTypes).visit(cu, ctx);

        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? DeleteMemberResult.changed(rewrittenSource)
                : DeleteMemberResult.noOp(sourceText, "No changes after formatting");
    }

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    /**
     * True when {@code stmt} is a deletable member matching {@code memberName}
     * and, when {@code paramTypes != null}, a method/constructor whose written
     * parameter types match in order.
     */
    private static boolean memberMatches(
            Statement stmt, String memberName, List<String> paramTypes, J.CompilationUnit cu) {
        if (stmt instanceof J.MethodDeclaration md) {
            if (!md.getSimpleName().equals(memberName)) {
                return false;
            }
            return paramTypes == null || paramTypesMatch(md, paramTypes, cu);
        }
        if (stmt instanceof J.VariableDeclarations vd) {
            // Fields are only candidates for name-only matches.
            if (paramTypes != null) {
                return false;
            }
            return vd.getVariables().stream()
                    .anyMatch(v -> v.getSimpleName().equals(memberName));
        }
        return false;
    }

    private static boolean paramTypesMatch(
            J.MethodDeclaration method, List<String> expectedTypes, J.CompilationUnit cu) {
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
     * Visitor that removes matching members from the target class body.
     *
     * <p>Uniqueness was already enforced for name-only deletes, so removing all
     * members satisfying the same predicate removes exactly the intended one.
     */
    private static final class MemberRemover
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetName;
        private final String memberName;
        private final List<String> paramTypes;

        MemberRemover(String targetName, String memberName, List<String> paramTypes) {
            this.targetName = targetName;
            this.memberName = memberName;
            this.paramTypes = paramTypes;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {
            if (!cd.getSimpleName().equals(targetName) || cd.getBody() == null) {
                return super.visitClassDeclaration(cd, ctx);
            }
            J.CompilationUnit enclosingCu = getCursor().firstEnclosing(J.CompilationUnit.class);
            List<Statement> kept = cd.getBody().getStatements().stream()
                    .filter(s -> !memberMatches(s, memberName, paramTypes, enclosingCu))
                    .collect(Collectors.toList());
            J.Block newBody = cd.getBody().withStatements(kept);
            J.ClassDeclaration updated = cd.withBody(newBody);
            return super.visitClassDeclaration(updated, ctx);
        }
    }
}
