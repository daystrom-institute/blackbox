package com.blackbox.javaworker;

import java.util.List;
import java.util.Optional;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.tree.J;

/**
 * Handles the {@code pruneUnusedImport} JSON-RPC method.
 *
 * <p>Removes each named import <strong>only if</strong> its symbol is no longer
 * referenced in the file, via OpenRewrite's {@code maybeRemoveImport} (which
 * performs the reference check). A still-referenced import is left untouched
 * (not an error); an absent one contributes nothing. Generic by construction.
 *
 * <p>Typical use: after a {@code ForEach}+{@code deleteMember} sweep removes the
 * last reference to an imported type (e.g. Apache Commons builders after the
 * hand-written equals/hashCode/toString are deleted in favor of Lombok).
 *
 * <p>Filesystem-free: source content arrives via params; output via JSON.
 */
final class PruneUnusedImportHandler {

    private final JavaParser parser;

    PruneUnusedImportHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    PruneUnusedImportResult handle(PruneUnusedImportParams params) {
        String sourceText = params.getSourceText();
        List<String> imports = params.getImports() != null ? params.getImports() : List.of();

        InMemoryExecutionContext ctx = new InMemoryExecutionContext(t -> { /* suppress */ });

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

        if (imports.isEmpty()) {
            return PruneUnusedImportResult.noOp(sourceText, "No imports requested");
        }

        J.CompilationUnit modified = (J.CompilationUnit) new ImportPruner(imports).visit(cu, ctx);

        String rewrittenSource = modified.printAll();
        boolean changed = !rewrittenSource.equals(sourceText);
        return changed
                ? PruneUnusedImportResult.changed(rewrittenSource)
                : PruneUnusedImportResult.noOp(sourceText, "No unused imports to remove");
    }

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    /**
     * Queues removal of each requested import via {@code maybeRemoveImport},
     * which only removes an import whose type is no longer referenced.
     */
    private static final class ImportPruner extends JavaIsoVisitor<InMemoryExecutionContext> {
        private final List<String> imports;

        ImportPruner(List<String> imports) {
            this.imports = imports;
        }

        @Override
        public J.CompilationUnit visitCompilationUnit(
                J.CompilationUnit cu, InMemoryExecutionContext ctx) {
            for (String fqn : imports) {
                maybeRemoveImport(fqn);
            }
            return super.visitCompilationUnit(cu, ctx);
        }
    }
}
