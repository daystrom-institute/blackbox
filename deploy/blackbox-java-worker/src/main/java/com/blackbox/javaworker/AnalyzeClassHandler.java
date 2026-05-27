package com.blackbox.javaworker;

import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import org.openrewrite.Cursor;
import org.openrewrite.InMemoryExecutionContext;
import org.openrewrite.SourceFile;
import org.openrewrite.java.JavaIsoVisitor;
import org.openrewrite.java.JavaParser;
import org.openrewrite.java.tree.J;
import org.openrewrite.java.tree.Statement;
import org.openrewrite.java.tree.TypeTree;

/**
 * Handles the {@code analyzeClass} JSON-RPC method — a generic structural
 * analysis of a Java type that reports facts useful to any accessor/boilerplate
 * codegen or migration tool (Lombok, Immutables, AutoValue, delombok, IDE
 * "generate getters/setters"). It carries no library-specific policy: it does
 * not mention Lombok and makes no decision about which annotation to apply.
 *
 * <p>The macro layer consumes these facts and owns the library mapping
 * (facts → Lombok annotations, {@code @Data}/{@code @Value} collapse, boolean
 * strategy). The analyzer's job is purely "what is the structure?".
 *
 * <h3>Reported facts (see {@link AnalyzeClassResult})</h3>
 * <ul>
 *   <li>instance fields (name, written type, final);</li>
 *   <li>trivial getters: {@code public T getX()/isX()} with body
 *       {@code return field;} / {@code return this.field;} whose return type
 *       matches the field type, no Javadoc — correlated to their field;</li>
 *   <li>trivial setters: {@code public void setX(T p)} with body
 *       {@code this.field = p;}, non-final field, no validation/Javadoc;</li>
 *   <li>coverage flags: do trivial getters cover every instance field; do
 *       trivial setters cover every non-final instance field;</li>
 *   <li>canonical constructors (no-arg / all-args / required-args);</li>
 *   <li>builder-delegating equals/hashCode/toString with the fields they cover
 *       in order (and whether they cover all fields);</li>
 *   <li>a conventional SLF4J logger field ({@code private static final Logger
 *       log = LoggerFactory.getLogger(X.class);});</li>
 *   <li>annotation simple-names already present on the type.</li>
 * </ul>
 */
final class AnalyzeClassHandler {

    private final JavaParser parser;

    AnalyzeClassHandler() {
        this.parser = JavaParser.fromJavaVersion().build();
    }

    AnalyzeClassResult handle(AnalyzeClassParams params) {
        String sourceText = params.getSourceText();
        String targetType = params.getTargetType();

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

        TargetFinder finder = new TargetFinder(targetType);
        finder.visit(cu, ctx);
        J.ClassDeclaration cd = finder.getFound();
        AnalyzeClassResult result = new AnalyzeClassResult();
        if (cd == null || cd.getBody() == null) {
            result.found = false;
            return result;
        }
        result.found = true;
        result.className = cd.getSimpleName();

        // Annotations already on the type.
        for (J.Annotation a : cd.getLeadingAnnotations()) {
            result.existingAnnotations.add(a.getSimpleName());
        }

        List<Statement> members = cd.getBody().getStatements();

        // -- Instance fields (in declaration order) -----------------------
        List<FieldFact> fields = new ArrayList<>();
        for (Statement s : members) {
            if (s instanceof J.VariableDeclarations vd) {
                boolean isStatic = vd.hasModifier(J.Modifier.Type.Static);
                if (isStatic) {
                    continue; // not an instance field
                }
                boolean isFinal = vd.hasModifier(J.Modifier.Type.Final);
                String type = typeOf(vd, cu);
                for (J.VariableDeclarations.NamedVariable v : vd.getVariables()) {
                    FieldFact f = new FieldFact();
                    f.name = v.getSimpleName();
                    f.type = type;
                    f.isFinal = isFinal;
                    fields.add(f);
                }
            }
        }
        result.fields = fields;

        // -- Trivial getters & setters ------------------------------------
        // Accessor-name-mismatch handling (e.g. getX on a boolean field whose
        // generated accessor is isX) follows the strategy:
        //   skip   — exclude the getter entirely (kept in source, not counted
        //            toward coverage, no annotation);
        //   bridge — include it, but rewrite its body to delegate to the
        //            generated accessor so callers of the old name still work;
        //   rename — include it as a normal delete (accept the name change).
        String strategy = params.getBooleanGetterStrategy();
        for (Statement s : members) {
            if (!(s instanceof J.MethodDeclaration md) || md.isConstructor()) {
                continue;
            }
            if (hasJavadoc(md)) {
                continue; // conservative: documented members are left alone
            }
            GetterFact g = asTrivialGetter(md, fields, cu);
            if (g != null) {
                if (g.apiMismatch) {
                    switch (strategy) {
                        case "skip" -> { continue; } // leave it; do not count it
                        case "bridge" -> {
                            g.bridge = true;
                            g.bridgeTo = g.lombokName;
                        }
                        default -> { } // "rename": normal delete
                    }
                }
                result.trivialGetters.add(g);
                continue;
            }
            SetterFact st = asTrivialSetter(md, fields, cu);
            if (st != null) {
                result.trivialSetters.add(st);
            }
        }

        // -- Coverage flags ------------------------------------------------
        Set<String> gotGetters = new LinkedHashSet<>();
        for (GetterFact g : result.trivialGetters) {
            gotGetters.add(g.field);
        }
        Set<String> gotSetters = new LinkedHashSet<>();
        for (SetterFact st : result.trivialSetters) {
            gotSetters.add(st.field);
        }
        boolean allGetters = !fields.isEmpty();
        boolean allSetters = true;
        boolean anyNonFinal = false;
        boolean allFinal = !fields.isEmpty();
        for (FieldFact f : fields) {
            if (!gotGetters.contains(f.name)) {
                allGetters = false;
            }
            if (f.isFinal) {
                allSetters = allSetters && true;
            } else {
                anyNonFinal = true;
                allFinal = false;
                if (!gotSetters.contains(f.name)) {
                    allSetters = false;
                }
            }
        }
        result.getterCoversAllFields = allGetters;
        result.setterCoversAllNonFinalFields = anyNonFinal && allSetters;
        result.allInstanceFieldsFinal = allFinal;

        // -- Canonical constructors ---------------------------------------
        List<String> fieldNamesInOrder = new ArrayList<>();
        List<String> finalFieldNamesInOrder = new ArrayList<>();
        for (FieldFact f : fields) {
            fieldNamesInOrder.add(f.name);
            if (f.isFinal) {
                finalFieldNamesInOrder.add(f.name);
            }
        }
        int noArgs = 0, allArgs = 0, reqArgs = 0;
        List<CtorFact> ctors = new ArrayList<>();
        for (Statement s : members) {
            if (s instanceof J.MethodDeclaration md && md.isConstructor() && !hasJavadoc(md)) {
                ConstructorKind k = classifyConstructor(md, fieldNamesInOrder, finalFieldNamesInOrder, cu);
                String kindStr = switch (k) {
                    case NO_ARGS -> { noArgs++; yield "no_args"; }
                    case ALL_ARGS -> { allArgs++; yield "all_args"; }
                    case REQUIRED_ARGS -> { reqArgs++; yield "required_args"; }
                    default -> null;
                };
                if (kindStr != null) {
                    CtorFact cf = new CtorFact();
                    cf.kind = kindStr;
                    cf.parameterTypes = paramTypesOf(md, cu);
                    ctors.add(cf);
                }
            }
        }
        // A collision (two of the same kind) disqualifies that kind.
        result.hasNoArgsConstructor = noArgs == 1;
        result.hasAllArgsConstructor = allArgs == 1;
        result.hasRequiredArgsConstructor = reqArgs == 1;
        // Expose only the non-colliding canonical constructors (the ones that
        // will actually be replaced by an annotation), with their param types.
        for (CtorFact cf : ctors) {
            boolean unique = switch (cf.kind) {
                case "no_args" -> noArgs == 1;
                case "all_args" -> allArgs == 1;
                case "required_args" -> reqArgs == 1;
                default -> false;
            };
            if (unique) {
                result.canonicalConstructors.add(cf);
            }
        }

        // -- Builder-delegating equals / hashCode / toString --------------
        for (Statement s : members) {
            if (!(s instanceof J.MethodDeclaration md) || md.isConstructor() || hasJavadoc(md)) {
                continue;
            }
            String name = md.getSimpleName();
            String body = md.getBody() == null ? "" :
                    md.getBody().printTrimmed(new Cursor(new Cursor(null, cu), md.getBody()));
            if (name.equals("equals")) {
                if (body.contains("EqualsBuilder")) {
                    result.equals = builderFact(body, fieldNamesInOrder);
                }
            } else if (name.equals("hashCode")) {
                if (body.contains("HashCodeBuilder")) {
                    result.hashCode = builderFact(body, fieldNamesInOrder);
                    // Custom seed (HashCodeBuilder(a, b)) preserves a hand-tuned
                    // distribution; flag it so the macro can decline @EqualsAndHashCode.
                    result.hashCode.customSeed = body.matches("(?s).*HashCodeBuilder\\s*\\(\\s*\\d+.*");
                }
            } else if (name.equals("toString")) {
                if (body.contains("ToStringBuilder")) {
                    result.toString = builderFact(body, fieldNamesInOrder);
                }
            }
        }

        // -- SLF4J logger field -------------------------------------------
        for (Statement s : members) {
            if (s instanceof J.VariableDeclarations vd
                    && vd.hasModifier(J.Modifier.Type.Static)
                    && vd.hasModifier(J.Modifier.Type.Final)) {
                String type = typeOf(vd, cu);
                for (J.VariableDeclarations.NamedVariable v : vd.getVariables()) {
                    if (v.getSimpleName().equals("log") && type.equals("Logger")
                            && v.getInitializer() != null) {
                        String init = v.getInitializer().printTrimmed(
                                new Cursor(new Cursor(null, cu), v.getInitializer()));
                        if (init.contains("LoggerFactory.getLogger")) {
                            result.loggerFieldPresent = true;
                            result.loggerFieldName = "log";
                        }
                    }
                }
            }
        }

        // -- Convenience booleans for macro guards ------------------------
        result.hasExistingAnnotations = !result.existingAnnotations.isEmpty();
        result.hasAnyTrivialGetter = !result.trivialGetters.isEmpty();
        result.hasAnyTrivialSetter = !result.trivialSetters.isEmpty();
        result.hasCanonicalConstructor =
                result.hasNoArgsConstructor || result.hasAllArgsConstructor
                        || result.hasRequiredArgsConstructor;

        boolean eqhcCovers = result.equals != null && result.equals.coversAll
                && result.hashCode != null && result.hashCode.coversAll
                && !result.hashCode.customSeed;
        boolean tostringCovers = result.toString != null && result.toString.coversAll;
        result.hasFinalField = fields.stream().anyMatch(f -> f.isFinal);

        // Matches lombokify's data_eligible exactly: the default data-class
        // annotation regenerates a required-args ctor (no-arg when no field is
        // final), so require that exact ctor — an all-args-only class does NOT
        // collapse (its all-args ctor stacks separately as @AllArgsConstructor).
        result.dataEligible =
                result.getterCoversAllFields
                        && result.setterCoversAllNonFinalFields
                        && eqhcCovers
                        && tostringCovers
                        && !result.hasExistingAnnotations
                        && (result.hasFinalField
                                ? result.hasRequiredArgsConstructor
                                : result.hasNoArgsConstructor);
        result.valueEligible =
                !result.dataEligible
                        && result.allInstanceFieldsFinal
                        && result.getterCoversAllFields
                        && !result.hasAnyTrivialSetter
                        && eqhcCovers
                        && tostringCovers
                        && result.hasAllArgsConstructor
                        && !result.hasExistingAnnotations;

        return result;
    }

    private Optional<SourceFile> parseOne(String src, InMemoryExecutionContext ctx) {
        parser.reset();
        return parser.parse(ctx, src).findFirst();
    }

    private static String typeOf(J.VariableDeclarations vd, J.CompilationUnit cu) {
        TypeTree tt = vd.getTypeExpression();
        return tt == null ? "" : tt.printTrimmed(new Cursor(new Cursor(null, cu), tt)).strip();
    }

    /** Written parameter types of a method/constructor, in order. */
    private static List<String> paramTypesOf(J.MethodDeclaration md, J.CompilationUnit cu) {
        List<String> types = new ArrayList<>();
        for (Statement p : md.getParameters()) {
            if (p instanceof J.VariableDeclarations pvd) {
                types.add(typeOf(pvd, cu));
            }
        }
        return types;
    }

    private static boolean hasJavadoc(J j) {
        return j.getComments().stream().anyMatch(c -> c instanceof org.openrewrite.java.tree.Javadoc.DocComment);
    }

    private static boolean isPublic(J.MethodDeclaration md) {
        return md.hasModifier(J.Modifier.Type.Public);
    }

    /** Returns a getter fact if {@code md} is a trivial getter for an instance field. */
    private static GetterFact asTrivialGetter(
            J.MethodDeclaration md, List<FieldFact> fields, J.CompilationUnit cu) {
        if (!isPublic(md) || md.getReturnTypeExpression() == null) {
            return null;
        }
        long paramCount = md.getParameters().stream().filter(p -> !(p instanceof J.Empty)).count();
        if (paramCount != 0 || md.getBody() == null) {
            return null;
        }
        List<Statement> stmts = md.getBody().getStatements();
        if (stmts.size() != 1 || !(stmts.get(0) instanceof J.Return ret) || ret.getExpression() == null) {
            return null;
        }
        String returned = ret.getExpression()
                .printTrimmed(new Cursor(new Cursor(null, cu), ret.getExpression())).strip();
        if (returned.startsWith("this.")) {
            returned = returned.substring("this.".length());
        }
        String name = md.getSimpleName();
        String retType = md.getReturnTypeExpression()
                .printTrimmed(new Cursor(new Cursor(null, cu), md.getReturnTypeExpression())).strip();
        for (FieldFact f : fields) {
            if (!f.name.equals(returned) || !f.type.equals(retType)) {
                continue;
            }
            String cap = capitalize(f.name);
            boolean isBoolean = retType.equals("boolean") || retType.equals("Boolean");
            if (name.equals("get" + cap) || (isBoolean && name.equals("is" + cap))) {
                GetterFact g = new GetterFact();
                g.method = name;
                g.field = f.name;
                g.type = retType;
                g.lombokName = generatedGetterName(f.name, retType);
                g.apiMismatch = !name.equals(g.lombokName);
                return g;
            }
        }
        return null;
    }

    /** Returns a setter fact if {@code md} is a trivial setter for a non-final field. */
    private static SetterFact asTrivialSetter(
            J.MethodDeclaration md, List<FieldFact> fields, J.CompilationUnit cu) {
        if (!isPublic(md) || md.getReturnTypeExpression() == null) {
            return null;
        }
        String retType = md.getReturnTypeExpression()
                .printTrimmed(new Cursor(new Cursor(null, cu), md.getReturnTypeExpression())).strip();
        if (!retType.equals("void")) {
            return null; // fluent setters (non-void) are left alone
        }
        List<Statement> params = md.getParameters().stream()
                .filter(p -> !(p instanceof J.Empty)).toList();
        if (params.size() != 1 || !(params.get(0) instanceof J.VariableDeclarations pvd)
                || md.getBody() == null) {
            return null;
        }
        String paramName = pvd.getVariables().get(0).getSimpleName();
        String paramType = typeOf(pvd, cu);
        List<Statement> stmts = md.getBody().getStatements();
        if (stmts.size() != 1 || !(stmts.get(0) instanceof J.Assignment asg)) {
            return null;
        }
        String lhs = asg.getVariable()
                .printTrimmed(new Cursor(new Cursor(null, cu), asg.getVariable())).strip();
        String rhs = asg.getAssignment()
                .printTrimmed(new Cursor(new Cursor(null, cu), asg.getAssignment())).strip();
        if (lhs.startsWith("this.")) {
            lhs = lhs.substring("this.".length());
        }
        if (!rhs.equals(paramName)) {
            return null; // body must be a plain assignment of the parameter
        }
        String name = md.getSimpleName();
        for (FieldFact f : fields) {
            if (f.isFinal || !f.name.equals(lhs) || !f.type.equals(paramType)) {
                continue;
            }
            if (name.equals("set" + capitalize(f.name))) {
                SetterFact st = new SetterFact();
                st.method = name;
                st.field = f.name;
                st.type = paramType;
                return st;
            }
        }
        return null;
    }

    private enum ConstructorKind { NONE, NO_ARGS, ALL_ARGS, REQUIRED_ARGS }

    private static ConstructorKind classifyConstructor(
            J.MethodDeclaration md, List<String> allFields, List<String> finalFields,
            J.CompilationUnit cu) {
        if (!isPublic(md) || md.getBody() == null) {
            return ConstructorKind.NONE;
        }
        List<Statement> params = md.getParameters().stream()
                .filter(p -> !(p instanceof J.Empty)).toList();
        List<Statement> stmts = md.getBody().getStatements();

        if (params.isEmpty()) {
            // No-arg: empty body or a single super();
            if (stmts.isEmpty()) {
                return ConstructorKind.NO_ARGS;
            }
            if (stmts.size() == 1) {
                String only = stmts.get(0)
                        .printTrimmed(new Cursor(new Cursor(null, cu), stmts.get(0))).strip();
                if (only.equals("super();")) {
                    return ConstructorKind.NO_ARGS;
                }
            }
            return ConstructorKind.NONE;
        }

        // Build the list of (paramName) in order and verify body is
        // this.f_i = p_i; for the matching field list, in declaration order.
        List<String> paramNames = new ArrayList<>();
        for (Statement p : params) {
            if (p instanceof J.VariableDeclarations pvd) {
                paramNames.add(pvd.getVariables().get(0).getSimpleName());
            } else {
                return ConstructorKind.NONE;
            }
        }
        if (stmts.size() != paramNames.size()) {
            return ConstructorKind.NONE;
        }
        List<String> assignedFields = new ArrayList<>();
        for (int i = 0; i < stmts.size(); i++) {
            if (!(stmts.get(i) instanceof J.Assignment asg)) {
                return ConstructorKind.NONE;
            }
            String lhs = asg.getVariable()
                    .printTrimmed(new Cursor(new Cursor(null, cu), asg.getVariable())).strip();
            String rhs = asg.getAssignment()
                    .printTrimmed(new Cursor(new Cursor(null, cu), asg.getAssignment())).strip();
            if (lhs.startsWith("this.")) {
                lhs = lhs.substring("this.".length());
            }
            if (!rhs.equals(paramNames.get(i))) {
                return ConstructorKind.NONE;
            }
            assignedFields.add(lhs);
        }
        if (assignedFields.equals(allFields)) {
            return ConstructorKind.ALL_ARGS;
        }
        if (assignedFields.equals(finalFields) && !finalFields.isEmpty()) {
            return ConstructorKind.REQUIRED_ARGS;
        }
        return ConstructorKind.NONE;
    }

    /** Extract the fields appended in a builder chain, in order, and coverage. */
    private static BuilderFact builderFact(String body, List<String> allFields) {
        BuilderFact bf = new BuilderFact();
        bf.present = true;
        // Collect identifiers appended via .append(<field>...) in chain order.
        // Matches `.append(name` and `.append("name", name` — first arg field or
        // second-arg field; we capture field names that are known instance fields.
        List<String> covered = new ArrayList<>();
        java.util.regex.Matcher m = java.util.regex.Pattern
                .compile("\\.append\\(\\s*(?:\"[^\"]*\"\\s*,\\s*)?([A-Za-z_][A-Za-z0-9_]*)")
                .matcher(body);
        while (m.find()) {
            String tok = m.group(1);
            // strip a leading this. if present is already excluded by the regex token
            if (allFields.contains(tok) && !covered.contains(tok)) {
                covered.add(tok);
            } else if (tok.equals("that") || tok.equals("other")) {
                // EqualsBuilder.append(field, that.field) — first arg is the field;
                // already captured by the first identifier. ignore the receiver.
            }
        }
        bf.fieldsInOrder = covered;
        bf.coversAll = !allFields.isEmpty() && covered.equals(allFields);
        return bf;
    }

    private static String capitalize(String s) {
        if (s.isEmpty()) {
            return s;
        }
        return Character.toUpperCase(s.charAt(0)) + s.substring(1);
    }

    /**
     * The accessor name a default getter-generator emits for {@code fieldName}
     * of {@code retType}: {@code isX} for primitive {@code boolean} (unless the
     * field is already {@code is}-prefixed), otherwise {@code getX}. Boxed
     * {@code Boolean} uses {@code getX}. Mirrors the convention the dissolved
     * lombokify kind used, so accessor-name mismatches are detected identically.
     */
    private static String generatedGetterName(String fieldName, String retType) {
        if (retType.equals("boolean")) {
            boolean startsWithIs = fieldName.startsWith("is")
                    && fieldName.length() > 2
                    && Character.isUpperCase(fieldName.charAt(2));
            return startsWithIs ? fieldName : "is" + capitalize(fieldName);
        }
        return "get" + capitalize(fieldName);
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
}
