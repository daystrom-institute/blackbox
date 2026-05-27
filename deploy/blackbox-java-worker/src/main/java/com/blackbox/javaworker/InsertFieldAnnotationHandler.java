package com.blackbox.javaworker;

import java.util.Comparator;
import java.util.List;
import java.util.Optional;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.JavaTemplate;
import org.openrewrite.java.tree.J;

/**
 * Handles the {@code insertFieldAnnotation} JSON-RPC method.
 *
 * <p>Adds an annotation immediately above a named field (per-field placement)
 * using OpenRewrite's {@link JavaTemplate}, and queues the supporting import.
 * The field-level analogue of {@link InsertClassAnnotationHandler}, for the
 * partial-coverage case where an annotation applies to one field rather than
 * the whole type.
 *
 * <h3>Genericity</h3>
 * <p>Carries no library-specific knowledge: the annotation text and imports are
 * caller data.
 *
 * <h3>Idempotency</h3>
 * <p>If the field already carries a leading annotation whose simple name matches
 * the requested annotation, the handler returns
 * {@link InsertFieldAnnotationResult#noOp} without inserting a duplicate.
 *
 * <p>Filesystem-free: source content arrives via params; output via JSON.
 */
final class InsertFieldAnnotationHandler {

    private final JavaParser parser;

    InsertFieldAnnotationHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    InsertFieldAnnotationResult handle(InsertFieldAnnotationParams params) {
        String sourceText = params.getSourceText();
        String targetType = params.getTargetType();
        String fieldName = params.getFieldName();
        String annotationText = params.getAnnotationText().strip();
        List<String> imports = params.getImports() != null ? params.getImports() : List.of();

        String newSimpleName = InsertClassAnnotationHandler.annotationSimpleName(annotationText);
        if (newSimpleName.isEmpty()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: annotation_text '" + annotationText
                    + "' is not a recognizable annotation");
        }

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

        // -- (2) Locate the field and check idempotency -------------------
        FieldFinder finder = new FieldFinder(targetType, fieldName);
        finder.visit(cu, ctx);
        J.VariableDeclarations field = finder.getFound();
        if (field == null) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Field '" + fieldName + "' not found in type '"
                    + targetType + "'");
        }
        for (J.Annotation existing : field.getLeadingAnnotations()) {
            if (existing.getSimpleName().equals(newSimpleName)) {
                return InsertFieldAnnotationResult.noOp(sourceText,
                        "Annotation '@" + newSimpleName + "' already present on field '"
                        + fieldName + "'");
            }
        }

        // -- (3) Insert the annotation and queue imports ------------------
        J.CompilationUnit modified = (J.CompilationUnit) new FieldAnnotationInserter(
                targetType, fieldName, annotationText, imports).visit(cu, ctx);

        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? InsertFieldAnnotationResult.changed(rewrittenSource)
                : InsertFieldAnnotationResult.noOp(sourceText, "No changes after formatting");
    }

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    /** Returns true when {@code vd} declares a variable named {@code fieldName}. */
    private static boolean declaresField(J.VariableDeclarations vd, String fieldName) {
        return vd.getVariables().stream().anyMatch(v -> v.getSimpleName().equals(fieldName));
    }

    /** Finds the field declaration named {@code fieldName} inside {@code targetType}. */
    private static final class FieldFinder extends JavaIsoVisitor<InMemoryExecutionContext> {
        private final String targetName;
        private final String fieldName;
        private J.VariableDeclarations found;
        private boolean inTarget;

        FieldFinder(String targetName, String fieldName) {
            this.targetName = targetName;
            this.fieldName = fieldName;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {
            boolean prev = inTarget;
            if (cd.getSimpleName().equals(targetName)) {
                inTarget = true;
            }
            J.ClassDeclaration r = super.visitClassDeclaration(cd, ctx);
            inTarget = prev;
            return r;
        }

        @Override
        public J.VariableDeclarations visitVariableDeclarations(
                J.VariableDeclarations vd, InMemoryExecutionContext ctx) {
            if (inTarget && found == null && declaresField(vd, fieldName)) {
                found = vd;
            }
            return super.visitVariableDeclarations(vd, ctx);
        }

        J.VariableDeclarations getFound() { return found; }
    }

    /** Adds the annotation to the named field via {@link JavaTemplate}. */
    private static final class FieldAnnotationInserter
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetName;
        private final String fieldName;
        private final String annotationText;
        private final List<String> imports;
        private boolean inTarget;

        FieldAnnotationInserter(
                String targetName, String fieldName, String annotationText, List<String> imports) {
            this.targetName = targetName;
            this.fieldName = fieldName;
            this.annotationText = annotationText;
            this.imports = imports;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {
            boolean prev = inTarget;
            if (cd.getSimpleName().equals(targetName)) {
                inTarget = true;
            }
            J.ClassDeclaration r = super.visitClassDeclaration(cd, ctx);
            inTarget = prev;
            return r;
        }

        @Override
        public J.VariableDeclarations visitVariableDeclarations(
                J.VariableDeclarations vd, InMemoryExecutionContext ctx) {
            if (!inTarget || !declaresField(vd, fieldName)) {
                return super.visitVariableDeclarations(vd, ctx);
            }

            JavaTemplate.Builder builder = JavaTemplate.builder(annotationText)
                    .javaParser(JavaParser.fromJavaVersion());
            if (!imports.isEmpty()) {
                builder = builder.imports(imports.toArray(new String[0]));
            }
            JavaTemplate template = builder.build();

            J.VariableDeclarations updated = template.apply(
                    getCursor(),
                    vd.getCoordinates().addAnnotation(Comparator.comparing(J.Annotation::getSimpleName)));

            // onlyIfReferenced=false: the annotation type is not on the worker
            // classpath, so it is not attributed; force the dedup'd import add.
            for (String fqn : imports) {
                maybeAddImport(fqn, false);
            }
            return super.visitVariableDeclarations(updated, ctx);
        }
    }
}
