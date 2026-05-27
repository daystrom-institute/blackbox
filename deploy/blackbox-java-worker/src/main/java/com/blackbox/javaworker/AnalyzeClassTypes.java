package com.blackbox.javaworker;

import com.fasterxml.jackson.annotation.JsonAutoDetect;
import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.annotation.JsonNaming;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/**
 * Request/response data classes for the {@code analyzeClass} method.
 *
 * <p>Result classes serialize their public fields in {@code snake_case} so the
 * Rust probe surfaces stable dotted paths (e.g. {@code analysis.trivial_getters})
 * that the macro layer's {@code ForEach}/predicates consume.
 */
@JsonIgnoreProperties(ignoreUnknown = true)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class AnalyzeClassParams {
    private final String targetFile;
    private final String sourceText;
    private final String targetType;

    @JsonCreator
    AnalyzeClassParams(
            @JsonProperty("target_file") String targetFile,
            @JsonProperty("source_text") String sourceText,
            @JsonProperty("target_type") String targetType) {
        this.targetFile = Objects.requireNonNull(targetFile, "target_file is required");
        this.sourceText = Objects.requireNonNull(sourceText, "source_text is required");
        this.targetType = Objects.requireNonNull(targetType, "target_type is required");
    }

    String getTargetFile() { return targetFile; }
    String getSourceText() { return sourceText; }
    String getTargetType() { return targetType; }
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class AnalyzeClassResult {
    public boolean found;
    public String className = "";
    public List<FieldFact> fields = new ArrayList<>();
    public List<GetterFact> trivialGetters = new ArrayList<>();
    public List<SetterFact> trivialSetters = new ArrayList<>();
    public boolean getterCoversAllFields;
    public boolean setterCoversAllNonFinalFields;
    public boolean allInstanceFieldsFinal;
    public boolean hasNoArgsConstructor;
    public boolean hasAllArgsConstructor;
    public boolean hasRequiredArgsConstructor;
    /** Canonical constructors detected, each with its written parameter types
     *  (for precise deletion via the macro's ForEach + deleteMember). */
    public List<CtorFact> canonicalConstructors = new ArrayList<>();
    public BuilderFact equals;
    public BuilderFact hashCode;
    public BuilderFact toString;
    public boolean loggerFieldPresent;
    public String loggerFieldName = "";
    public List<String> existingAnnotations = new ArrayList<>();
    // Convenience booleans for macro guards (the bounded predicate grammar
    // cannot test array emptiness/length). Generic facts, no library policy.
    public boolean hasExistingAnnotations;
    public boolean hasAnyTrivialGetter;
    public boolean hasAnyTrivialSetter;
    public boolean hasCanonicalConstructor;
    /** True when ≥1 instance field is final. */
    public boolean hasFinalField;
    /** Structural summary for a MUTABLE plain-data class whose default-arg
     *  constructor is canonical: full getter+setter coverage, builder
     *  equals/hashCode (no custom seed) + toString over all fields, no
     *  pre-existing annotations, not all-final, and the no-arg/required-args
     *  constructor that a default data-class annotation regenerates is present
     *  (required-args when any field is final, else no-args). The macro maps
     *  this to its library's mutable data-class annotation. */
    public boolean dataEligible;
    /** Structural summary for an IMMUTABLE plain-data class: all fields final,
     *  full getter coverage, no setters, builder equals/hashCode/toString over
     *  all fields, an all-args constructor, no pre-existing annotations. */
    public boolean valueEligible;
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class FieldFact {
    public String name = "";
    public String type = "";
    public boolean isFinal;
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class GetterFact {
    public String method = "";
    public String field = "";
    public String type = "";
    /** {@code isX()} form on a boolean field. */
    public boolean booleanIsForm;
    /** {@code getX()} form on a boolean field (subject to the boolean strategy). */
    public boolean booleanGetFormOnBoolean;
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class SetterFact {
    public String method = "";
    public String field = "";
    public String type = "";
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class CtorFact {
    /** {@code no_args} | {@code all_args} | {@code required_args}. */
    public String kind = "";
    public List<String> parameterTypes = new ArrayList<>();
}

@JsonAutoDetect(fieldVisibility = JsonAutoDetect.Visibility.ANY)
@JsonNaming(PropertyNamingStrategies.SnakeCaseStrategy.class)
final class BuilderFact {
    public boolean present;
    public List<String> fieldsInOrder = new ArrayList<>();
    public boolean coversAll;
    public boolean customSeed;
}
