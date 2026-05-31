//! Golden regression tests for the `builtin.java.lombok` macro.
//!
//! These began as a parity proof against the `lombokify_java_class` Rust kind
//! (the deletion gate). After parity was proven on every transformation path
//! and the conservative-decline edges, that Rust kind was dissolved and these
//! were converted to standalone golden assertions over the macro's output —
//! the same scenarios, now self-contained (no dependency on the deleted kind).
//!
//! Skipped when `BLACKBOX_JAVA_WORKER_JAR` is unset (the macro path needs the
//! live OpenRewrite worker).

#![cfg(test)]

use std::path::{Path, PathBuf};

use crate::macros::backend::JavaMacroBackend;
use crate::macros::model::MacroInvocation;
use crate::macros::planner::MacroPlanner;
use crate::macros::planner_ctx::MacroPlannerContext;
use crate::macros::probe::CodeNavProbeRunner;
use crate::macros::registry::MacroRegistry;
use crate::macros::sidecar_backend::SidecarBackend;

fn jar_ready() -> bool {
    std::env::var("BLACKBOX_JAVA_WORKER_JAR")
        .ok()
        .filter(|j| !j.trim().is_empty())
        .map(|j| PathBuf::from(&j).exists())
        .unwrap_or(false)
}

/// Run the `builtin.java.lombok` macro and return the rewritten source for
/// `file` (or the original content when the macro produces no edit).
fn run_macro(project_dir: &Path, file: &Path, target_type: &str) -> String {
    run_macro_strategy(project_dir, file, target_type, None)
}

/// As [`run_macro`], optionally supplying `boolean_getter_strategy`.
fn run_macro_strategy(
    project_dir: &Path,
    file: &Path,
    target_type: &str,
    boolean_getter_strategy: Option<&str>,
) -> String {
    let def = MacroRegistry::get(None, "builtin.java.lombok")
        .expect("registry get must not error")
        .expect("builtin.java.lombok must be registered");

    let mut inputs = serde_json::Map::new();
    inputs.insert("file".into(), serde_json::json!(file.to_string_lossy()));
    inputs.insert("target_type".into(), serde_json::json!(target_type));
    if let Some(strat) = boolean_getter_strategy {
        inputs.insert("boolean_getter_strategy".into(), serde_json::json!(strat));
    }

    let inv = MacroInvocation {
        macro_id: "builtin.java.lombok".into(),
        version: None,
        project_dir: project_dir.to_string_lossy().into_owned(),
        inputs,
        anchors: None,
        operator_opt_outs: vec![],
    };

    let backend: Box<dyn JavaMacroBackend> =
        Box::new(SidecarBackend::new(project_dir.to_path_buf()));
    let project_record = crate::projects::ProjectRecord {
        project_id: "lombok-golden".into(),
        repo_id: None,
        canonical_path: project_dir.to_string_lossy().into_owned(),
        registered_at: "2024-01-01T00:00:00Z".into(),
        is_git_repo: false,
        languages: std::collections::BTreeSet::new(),
    };
    let runner = CodeNavProbeRunner::new(None, vec![project_record]);
    let ctx = MacroPlannerContext::new(backend, None, Box::new(runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("macro plan must succeed");
    assert!(
        plan.refusals.is_empty(),
        "macro must not refuse this scenario; got: {:?}",
        plan.refusals
    );

    let file_str = file.to_string_lossy();
    match plan
        .edits
        .file_edits
        .iter()
        .find(|fe| file_str.ends_with(fe.path.as_str()) || fe.path == file_str)
        .or_else(|| plan.edits.file_edits.last())
    {
        Some(fe) => fe
            .new_text
            .clone()
            .unwrap_or_else(|| std::fs::read_to_string(file).unwrap()),
        None => std::fs::read_to_string(file).unwrap(),
    }
}

/// Write `src` to `<tmp>/<Type>.java`, run the macro, return the rewritten
/// source. Returns `None` (test should early-return) when the worker is absent.
fn macro_output(scenario: &str, type_name: &str, src: &str) -> Option<String> {
    if !jar_ready() {
        eprintln!("[lombok_golden] worker JAR unavailable — skipping '{scenario}'");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!("{type_name}.java"));
    std::fs::write(&file, src).unwrap();
    Some(run_macro(dir.path(), &file, type_name))
}

/// As [`macro_output`], with an explicit `boolean_getter_strategy`.
fn macro_output_strategy(
    scenario: &str,
    type_name: &str,
    src: &str,
    strategy: &str,
) -> Option<String> {
    if !jar_ready() {
        eprintln!("[lombok_golden] worker JAR unavailable — skipping '{scenario}'");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!("{type_name}.java"));
    std::fs::write(&file, src).unwrap();
    Some(run_macro_strategy(
        dir.path(),
        &file,
        type_name,
        Some(strategy),
    ))
}

/// A primitive `boolean` field with a hand-rolled `getActive()` — the generated
/// accessor would be `isActive()`, so dropping `getActive()` is an API change.
const BOOLEAN_MISMATCH_SRC: &str = "package com.example;\n\n\
     public class Toggle {\n\
     \x20   private boolean active;\n\n\
     \x20   public boolean getActive() { return active; }\n\
     }\n";

#[test]
fn macro_bulk_by_composition_over_directory() {
    // Bulk/directory lombokify dissolves into "invoke the per-class macro once
    // per discovered class" — the dissolution separates the per-class
    // transformation (the macro) from iteration (orchestration). This proves
    // that composition: a directory with two lombokifiable classes and one
    // non-POJO. The POJOs are transformed; the non-POJO is a non-fatal no-op
    // (lombokify's "leftover"), not an error.
    if !jar_ready() {
        eprintln!("[lombok_golden] worker JAR unavailable — skipping bulk composition");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg).unwrap();

    let pair = pkg.join("Pair.java");
    std::fs::write(
        &pair,
        "package com.example;\n\npublic class Pair {\n\
         \x20   private int a;\n\
         \x20   public int getA() { return a; }\n}\n",
    )
    .unwrap();
    let plain = pkg.join("Plain.java");
    std::fs::write(
        &plain,
        "package com.example;\n\npublic class Plain {\n\
         \x20   private String name;\n\
         \x20   public String getName() { return name; }\n}\n",
    )
    .unwrap();
    // A non-POJO: a method with real logic, no trivial accessors.
    let svc = pkg.join("Service.java");
    let svc_src = "package com.example;\n\npublic class Service {\n\
         \x20   public int compute(int x) { return x * x + 1; }\n}\n";
    std::fs::write(&svc, svc_src).unwrap();

    // Caller-side iteration (what an orchestration loop / bulk runner does):
    // invoke the per-class macro for each discovered (file, class).
    let out_pair = run_macro(dir.path(), &pair, "Pair");
    let out_plain = run_macro(dir.path(), &plain, "Plain");
    let out_svc = run_macro(dir.path(), &svc, "Service");

    assert!(
        out_pair.contains("@Getter") && !out_pair.contains("getA()"),
        "Pair must be lombokified; got:\n{out_pair}"
    );
    assert!(
        out_plain.contains("@Getter") && !out_plain.contains("getName()"),
        "Plain must be lombokified; got:\n{out_plain}"
    );
    // Leftover: the non-POJO is untouched (no-op), not an error.
    assert_eq!(
        out_svc, svc_src,
        "non-POJO Service must be a non-fatal no-op (unchanged); got:\n{out_svc}"
    );
}

#[test]
fn macro_boolean_getter_skip_leaves_mismatch_untouched() {
    // skip (default): getActive is excluded → no coverage → nothing happens.
    let Some(out) = macro_output_strategy("bool_skip", "Toggle", BOOLEAN_MISMATCH_SRC, "skip")
    else {
        return;
    };
    assert!(
        !out.contains("@Getter"),
        "skip must not add @Getter; got:\n{out}"
    );
    assert!(
        out.contains("public boolean getActive()"),
        "getActive must remain; got:\n{out}"
    );
    assert!(
        !out.contains("lombok"),
        "no lombok import under skip; got:\n{out}"
    );
}

#[test]
fn macro_boolean_getter_bridge_delegates_to_generated() {
    // bridge: @Getter generates isActive(); getActive() is kept but rewritten to
    // delegate, so existing callers of getActive() still compile.
    let Some(out) = macro_output_strategy("bool_bridge", "Toggle", BOOLEAN_MISMATCH_SRC, "bridge")
    else {
        return;
    };
    assert!(
        out.contains("@Getter"),
        "bridge must add @Getter; got:\n{out}"
    );
    assert!(
        out.contains("import lombok.Getter;"),
        "bridge must add the import; got:\n{out}"
    );
    assert!(
        out.contains("public boolean getActive()"),
        "bridge keeps getActive(); got:\n{out}"
    );
    // The body now delegates to the generated accessor.
    let normalized: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("return isActive();"),
        "bridge body must delegate to isActive(); got:\n{out}"
    );
}

#[test]
fn macro_boolean_getter_rename_drops_mismatch() {
    // rename: @Getter generates isActive(); getActive() is dropped (callers
    // accept the rename).
    let Some(out) = macro_output_strategy("bool_rename", "Toggle", BOOLEAN_MISMATCH_SRC, "rename")
    else {
        return;
    };
    assert!(
        out.contains("@Getter"),
        "rename must add @Getter; got:\n{out}"
    );
    assert!(out.contains("import lombok.Getter;"));
    assert!(
        !out.contains("getActive"),
        "rename must drop getActive(); got:\n{out}"
    );
}

#[test]
fn macro_getters_setters_allargs_ctor() {
    let Some(out) = macro_output(
        "getters_setters_allargs",
        "Point",
        "package com.example;\n\n\
         public class Point {\n\
         \x20   private int x;\n\
         \x20   private int y;\n\n\
         \x20   public Point(int x, int y) {\n\
         \x20       this.x = x;\n\
         \x20       this.y = y;\n\
         \x20   }\n\n\
         \x20   public int getX() { return x; }\n\
         \x20   public int getY() { return y; }\n\
         \x20   public void setX(int x) { this.x = x; }\n\
         \x20   public void setY(int y) { this.y = y; }\n\
         }\n",
    ) else {
        return;
    };
    for ann in ["@Getter", "@Setter", "@AllArgsConstructor"] {
        assert!(out.contains(ann), "expected {ann}; got:\n{out}");
    }
    for imp in [
        "import lombok.Getter;",
        "import lombok.Setter;",
        "import lombok.AllArgsConstructor;",
    ] {
        assert!(out.contains(imp), "expected {imp}; got:\n{out}");
    }
    // Boilerplate removed; fields preserved.
    assert!(!out.contains("getX()"), "getX must be removed; got:\n{out}");
    assert!(!out.contains("setY("), "setY must be removed; got:\n{out}");
    assert!(
        !out.contains("public Point("),
        "ctor must be removed; got:\n{out}"
    );
    assert!(out.contains("private int x;") && out.contains("private int y;"));
}

#[test]
fn macro_data_collapse() {
    let Some(out) = macro_output(
        "data_collapse",
        "Bean",
        "package com.example;\n\n\
         import org.apache.commons.lang3.builder.EqualsBuilder;\n\
         import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
         import org.apache.commons.lang3.builder.ToStringBuilder;\n\n\
         public class Bean {\n\
         \x20   private String name;\n\
         \x20   private int count;\n\n\
         \x20   public Bean() {}\n\n\
         \x20   public String getName() { return name; }\n\
         \x20   public int getCount() { return count; }\n\
         \x20   public void setName(String name) { this.name = name; }\n\
         \x20   public void setCount(int count) { this.count = count; }\n\n\
         \x20   public boolean equals(Object other) {\n\
         \x20       if (this == other) return true;\n\
         \x20       Bean that = (Bean) other;\n\
         \x20       return new EqualsBuilder().append(name, that.name).append(count, that.count).isEquals();\n\
         \x20   }\n\
         \x20   public int hashCode() {\n\
         \x20       return new HashCodeBuilder().append(name).append(count).toHashCode();\n\
         \x20   }\n\
         \x20   public String toString() {\n\
         \x20       return new ToStringBuilder(this).append(\"name\", name).append(\"count\", count).toString();\n\
         \x20   }\n\
         }\n",
    ) else {
        return;
    };
    assert!(
        out.contains("@Data"),
        "expected @Data collapse; got:\n{out}"
    );
    assert!(out.contains("import lombok.Data;"));
    // The individual annotations must NOT appear (collapsed into @Data).
    for ann in ["@Getter", "@Setter", "@EqualsAndHashCode", "@ToString"] {
        assert!(
            !out.contains(ann),
            "{ann} must be collapsed into @Data; got:\n{out}"
        );
    }
    // Boilerplate + now-unused Apache imports removed.
    assert!(
        !out.contains("EqualsBuilder"),
        "EqualsBuilder must be gone; got:\n{out}"
    );
    assert!(!out.contains("getName()") && !out.contains("hashCode()"));
    assert!(out.contains("private String name;") && out.contains("private int count;"));
}

#[test]
fn macro_value_collapse() {
    let Some(out) = macro_output(
        "value_collapse",
        "Money",
        "package com.example;\n\n\
         import org.apache.commons.lang3.builder.EqualsBuilder;\n\
         import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
         import org.apache.commons.lang3.builder.ToStringBuilder;\n\n\
         public class Money {\n\
         \x20   private final String currency;\n\
         \x20   private final long amount;\n\n\
         \x20   public Money(String currency, long amount) {\n\
         \x20       this.currency = currency;\n\
         \x20       this.amount = amount;\n\
         \x20   }\n\n\
         \x20   public String getCurrency() { return currency; }\n\
         \x20   public long getAmount() { return amount; }\n\n\
         \x20   public boolean equals(Object other) {\n\
         \x20       if (this == other) return true;\n\
         \x20       Money that = (Money) other;\n\
         \x20       return new EqualsBuilder().append(currency, that.currency).append(amount, that.amount).isEquals();\n\
         \x20   }\n\
         \x20   public int hashCode() {\n\
         \x20       return new HashCodeBuilder().append(currency).append(amount).toHashCode();\n\
         \x20   }\n\
         \x20   public String toString() {\n\
         \x20       return new ToStringBuilder(this).append(\"currency\", currency).append(\"amount\", amount).toString();\n\
         \x20   }\n\
         }\n",
    ) else {
        return;
    };
    assert!(
        out.contains("@Value"),
        "expected @Value collapse; got:\n{out}"
    );
    assert!(out.contains("import lombok.Value;"));
    assert!(
        !out.contains("@AllArgsConstructor"),
        "@AllArgsConstructor implied by @Value; got:\n{out}"
    );
    assert!(!out.contains("EqualsBuilder") && !out.contains("getCurrency()"));
    assert!(out.contains("private final String currency;"));
}

#[test]
fn macro_partial_per_field_getter() {
    let Some(out) = macro_output(
        "partial_per_field_getter",
        "Mix",
        "package com.example;\n\n\
         public class Mix {\n\
         \x20   private int first;\n\
         \x20   private int second;\n\n\
         \x20   public int getFirst() { return first; }\n\
         \x20   public int getSecond() { return second * 2; }\n\
         }\n",
    ) else {
        return;
    };
    // Partial coverage → @Getter lands per-field on `first` (exactly once),
    // never class-level; the computed getSecond stays.
    assert!(
        out.contains("@Getter"),
        "expected per-field @Getter; got:\n{out}"
    );
    assert_eq!(
        out.matches("@Getter").count(),
        1,
        "exactly one @Getter; got:\n{out}"
    );
    assert!(
        !out.contains("getFirst()"),
        "getFirst must be removed; got:\n{out}"
    );
    assert!(
        out.contains("getSecond()"),
        "computed getSecond must remain; got:\n{out}"
    );
}

#[test]
fn macro_slf4j_logger_keeps_slf4j_imports() {
    let Some(out) = macro_output(
        "slf4j_logger",
        "Svc",
        "package com.example;\n\n\
         import org.slf4j.Logger;\n\
         import org.slf4j.LoggerFactory;\n\n\
         public class Svc {\n\
         \x20   private static final Logger log = LoggerFactory.getLogger(Svc.class);\n\
         \x20   private String name;\n\n\
         \x20   public String getName() { return name; }\n\
         }\n",
    ) else {
        return;
    };
    assert!(out.contains("@Slf4j") && out.contains("@Getter"));
    assert!(out.contains("import lombok.extern.slf4j.Slf4j;"));
    assert!(
        !out.contains("LoggerFactory.getLogger"),
        "logger field must be removed; got:\n{out}"
    );
    // Parity with lombokify: slf4j imports are intentionally left, not pruned.
    assert!(
        out.contains("import org.slf4j.Logger;") && out.contains("import org.slf4j.LoggerFactory;"),
        "slf4j imports must be kept (not pruned); got:\n{out}"
    );
}

#[test]
fn macro_custom_hashcode_seed_keeps_methods() {
    let src = "package com.example;\n\n\
         import org.apache.commons.lang3.builder.EqualsBuilder;\n\
         import org.apache.commons.lang3.builder.HashCodeBuilder;\n\n\
         public class Seeded {\n\
         \x20   private String id;\n\n\
         \x20   public boolean equals(Object other) {\n\
         \x20       if (this == other) return true;\n\
         \x20       Seeded that = (Seeded) other;\n\
         \x20       return new EqualsBuilder().append(id, that.id).isEquals();\n\
         \x20   }\n\
         \x20   public int hashCode() {\n\
         \x20       return new HashCodeBuilder(17, 37).append(id).toHashCode();\n\
         \x20   }\n\
         }\n";
    let Some(out) = macro_output("custom_hashcode_seed", "Seeded", src) else {
        return;
    };
    // Custom seed → decline @EqualsAndHashCode and keep both methods + imports.
    assert!(
        !out.contains("@EqualsAndHashCode"),
        "must decline @EqualsAndHashCode; got:\n{out}"
    );
    assert!(
        out.contains("HashCodeBuilder(17, 37)"),
        "hashCode must be preserved; got:\n{out}"
    );
    assert!(out.contains("import org.apache.commons.lang3.builder.EqualsBuilder;"));
}

#[test]
fn macro_javadoc_getter_skipped() {
    let src = "package com.example;\n\n\
         public class Documented {\n\
         \x20   private String name;\n\n\
         \x20   /** Returns the name. */\n\
         \x20   public String getName() { return name; }\n\
         }\n";
    let Some(out) = macro_output("javadoc_getter_skipped", "Documented", src) else {
        return;
    };
    // Documented getter is not trivial → no change.
    assert!(
        !out.contains("@Getter"),
        "documented getter must be skipped; got:\n{out}"
    );
    assert!(
        out.contains("public String getName()"),
        "getter must remain; got:\n{out}"
    );
}

#[test]
fn macro_validation_setter_skipped() {
    let src = "package com.example;\n\n\
         import java.util.Objects;\n\n\
         public class Guarded {\n\
         \x20   private String name;\n\n\
         \x20   public String getName() { return name; }\n\
         \x20   public void setName(String name) { this.name = Objects.requireNonNull(name); }\n\
         }\n";
    let Some(out) = macro_output("validation_setter_skipped", "Guarded", src) else {
        return;
    };
    // Getter is trivial (sole field → class-level @Getter); validation setter declined.
    assert!(
        out.contains("@Getter"),
        "trivial getter should collapse; got:\n{out}"
    );
    assert!(
        !out.contains("@Setter"),
        "validation setter must be declined; got:\n{out}"
    );
    assert!(
        out.contains("Objects.requireNonNull"),
        "validation setter must remain; got:\n{out}"
    );
}
