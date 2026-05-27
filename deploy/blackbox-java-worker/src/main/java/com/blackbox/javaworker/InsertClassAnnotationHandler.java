package com.blackbox.javaworker;

import java.util.Comparator;
import java.util.List;
import java.util.Optional;
import org.openrewrite.ExecutionContext;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.JavaTemplate;
import org.openrewrite.java.tree.J;

/**
 * Handles the {@code insertClassAnnotation} JSON-RPC method.
 *
 * <p>Adds an annotation to a type declaration (class-level placement) using
 * OpenRewrite's {@link JavaTemplate} so the annotation lands on its own line
 * above the type with correct layout, and queues the supporting import via
 * {@code maybeAddImport} (which deduplicates).
 *
 * <h3>Genericity</h3>
 * <p>This handler carries no library-specific knowledge. The annotation text
 * and its imports are supplied by the caller (macro data), so any
 * annotation-codegen library (Lombok {@code @Getter}/{@code @Data}, Immutables
 * {@code @Value.Immutable}, AutoValue, MapStruct, …) is driven by passing the
 * appropriate {@code annotation_text} + {@code imports}.
 *
 * <h3>Idempotency</h3>
 * <p>If the target type already carries a leading annotation whose simple name
 * matches the requested annotation, the handler returns
 * {@link InsertClassAnnotationResult#noOp} without inserting a duplicate.
 *
 * <p>Filesystem-free: all source content arrives via params; all output goes
 * via JSON response. The Rust client owns file I/O and transactions.
 */
final class InsertClassAnnotationHandler {

    private final JavaParser parser;

    InsertClassAnnotationHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    /**
     * Add a class-level annotation to the target type within the given source.
     *
     * @param params validated insert-class-annotation parameters
     * @return result with rewritten source, change flags, and diagnostics
     * @throws SidecarException with {@link SidecarException#PARSE_INVALID} when
     *     the source does not parse or the target type is not found
     */
    InsertClassAnnotationResult handle(InsertClassAnnotationParams params) {
        String sourceText = params.getSourceText();
        String targetType = params.getTargetType();
        String annotationText = params.getAnnotationText().strip();
        List<String> imports = params.getImports() != null ? params.getImports() : List.of();

        String newSimpleName = annotationSimpleName(annotationText);
        if (newSimpleName.isEmpty()) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: annotation_text '" + annotationText
                    + "' is not a recognizable annotation");
        }

        InMemoryExecutionContext ctx = new InMemoryExecutionContext(t -> { /* suppress stderr */ });

        // -- (1) Parse existing source ------------------------------------
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

        // -- (2) Find target type and check idempotency -------------------
        TargetFinder finder = new TargetFinder(targetType);
        finder.visit(cu, ctx);
        J.ClassDeclaration targetCd = finder.getFound();
        if (targetCd == null) {
            throw new SidecarException(SidecarException.PARSE_INVALID,
                    "error.parse_invalid: Target type '" + targetType + "' not found in source");
        }
        for (J.Annotation existing : targetCd.getLeadingAnnotations()) {
            if (existing.getSimpleName().equals(newSimpleName)) {
                return InsertClassAnnotationResult.noOp(sourceText,
                        "Annotation '@" + newSimpleName + "' already present on " + targetType);
            }
        }

        // -- (3) Insert the annotation and queue imports ------------------
        J.CompilationUnit modified = (J.CompilationUnit) new AnnotationInserter(
                targetType, annotationText, imports).visit(cu, ctx);

        // -- (4) Format and return ----------------------------------------
        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? InsertClassAnnotationResult.changed(rewrittenSource)
                : InsertClassAnnotationResult.noOp(sourceText, "No changes after formatting");
    }

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    /**
     * Derive the simple name of an annotation from its source text.
     *
     * <p>Examples: {@code "@Getter"} → {@code "Getter"};
     * {@code "@EqualsAndHashCode(callSuper = false)"} → {@code "EqualsAndHashCode"};
     * {@code "@lombok.extern.slf4j.Slf4j"} → {@code "Slf4j"}.
     */
    static String annotationSimpleName(String annotationText) {
        String s = annotationText.strip();
        if (s.startsWith("@")) {
            s = s.substring(1);
        }
        // Cut argument list, if any.
        int paren = s.indexOf('(');
        if (paren >= 0) {
            s = s.substring(0, paren);
        }
        s = s.strip();
        // Cut whitespace tail (defensive).
        int ws = s.indexOf(' ');
        if (ws >= 0) {
            s = s.substring(0, ws);
        }
        // Last dotted segment for fully-qualified annotation references.
        int dot = s.lastIndexOf('.');
        if (dot >= 0) {
            s = s.substring(dot + 1);
        }
        return s.strip();
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
     * Visitor that adds the annotation to the target class via
     * {@link JavaTemplate} and queues the supporting imports.
     *
     * <p>{@code addAnnotation} with a name comparator lets OpenRewrite place the
     * new annotation in a stable order relative to existing annotations and
     * format it on its own line. {@code maybeAddImport} deduplicates imports.
     */
    private static final class AnnotationInserter
            extends JavaIsoVisitor<InMemoryExecutionContext> {

        private final String targetName;
        private final String annotationText;
        private final List<String> imports;

        AnnotationInserter(String targetName, String annotationText, List<String> imports) {
            this.targetName = targetName;
            this.annotationText = annotationText;
            this.imports = imports;
        }

        @Override
        public J.ClassDeclaration visitClassDeclaration(
                J.ClassDeclaration cd, InMemoryExecutionContext ctx) {
            if (!cd.getSimpleName().equals(targetName)) {
                return super.visitClassDeclaration(cd, ctx);
            }

            JavaTemplate.Builder builder = JavaTemplate.builder(annotationText)
                    .javaParser(JavaParser.fromJavaVersion());
            if (!imports.isEmpty()) {
                builder = builder.imports(imports.toArray(new String[0]));
            }
            JavaTemplate template = builder.build();

            J.ClassDeclaration updated = template.apply(
                    getCursor(),
                    cd.getCoordinates().addAnnotation(Comparator.comparing(J.Annotation::getSimpleName)));

            // onlyIfReferenced=false is required: the annotation's type
            // (e.g. lombok.Getter) is not on the worker classpath, so it is not
            // type-attributed and OpenRewrite would not see it as "referenced".
            // The caller explicitly supplies the import to pair with the
            // annotation, so force the addition (maybeAddImport still dedups).
            for (String fqn : imports) {
                maybeAddImport(fqn, false);
            }

            // Descend into the updated node's children; the target match above
            // is by simple name so nested same-named types are out of scope for v1.
            return super.visitClassDeclaration(updated, ctx);
        }
    }
}
