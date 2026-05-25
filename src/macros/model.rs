//! Core data model for the macro layer.
//!
//! `MacroDefinition` — a registered recipe (data, not plugin code).
//! `MacroInvocation` — a typed invocation of a macro with inputs and anchors.
//! `MacroPlan`       — the review artifact produced by macro planning.
//! `EditSet`         — the edit payload that lowers to a `RefactorPlan`.
//!
//! # IR lowering
//!
//! ```text
//! MacroDefinition + typed inputs + probes
//!    → MacroPlan          (review artifact)
//!    → lowers to →
//!    RefactorPlan         (file_moves, edits, file_creates, validations)
//!    → refactor::apply()  (the sole writer; all safety guards live here)
//! ```
//!
//! `EditSet` maps 1:1 onto `RefactorPlan` as follows:
//! - `EditSet.file_edits`   → `RefactorPlan.edits`
//! - `EditSet.file_creates` → `RefactorPlan.file_creates`
//! - `EditSet.file_moves`   → `RefactorPlan.file_moves`
//! - `EditSet.backends_used` is preserved on `MacroPlan.backends_used` and
//!   promoted to `RefactorPlan` title/provenance metadata at lowering time.

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::macros::expr::Predicate;
use crate::refactor::{FileCreate, FileEdit, FileMove};

// ---------------------------------------------------------------------------
// MacroScope
// ---------------------------------------------------------------------------

/// Where a macro definition originates. Controls trust and search order.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MacroScope {
    /// Ships with Blackbox. Highest trust.
    Builtin,
    /// Defined by the operator in user config. Medium trust.
    User,
    /// Defined in `.bbox/macros/` inside a project repo. Reviewed like source.
    Project,
}

// ---------------------------------------------------------------------------
// MacroDefinition
// ---------------------------------------------------------------------------

/// A registered macro recipe. Data-only — no executable code.
///
/// Builtins ship with Blackbox. Project macros live under `.bbox/macros/`
/// so teams can review them like source. User macros live in operator config.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroDefinition {
    /// Stable unique identifier, e.g. `"java.add_service_boundary"`.
    pub id: String,
    /// Semver or monotonic version string.
    pub version: String,
    /// Target language, e.g. `"java"`, `"rust"`, `"any"`.
    pub language: String,
    pub scope: MacroScope,
    /// Short human-readable description.
    pub title: String,
    /// JSON Schema object describing the `inputs` map for invocations.
    pub inputs_schema: Value,
    /// Declared effect tags, e.g. `["creates_type", "changes_dependency_injection"]`.
    #[serde(default)]
    pub effects: Vec<String>,
    /// Authority-gate names required for destructive/public-API effects.
    /// Agents may pass through operator-supplied gates but must not infer them
    /// (RX-V1).
    #[serde(default)]
    pub authority_gates: Vec<String>,
    /// Named probe slots declared by this macro. Evaluated at plan time.
    #[serde(default)]
    pub probes: Vec<MacroProbe>,
    /// Ordered operations this macro will perform.
    #[serde(default)]
    pub operations: Vec<MacroOperation>,
    /// Post-apply validation rules.
    #[serde(default)]
    pub validations: Vec<MacroValidation>,
    /// Conditional refusal rules. Checked before any planning work.
    #[serde(default)]
    pub refusals: Vec<MacroRefusal>,
}

// ---------------------------------------------------------------------------
// MacroAnchors
// ---------------------------------------------------------------------------

/// Optional source anchors for a macro invocation: files, symbols, and/or
/// byte/line ranges the macro should operate on.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct MacroAnchors {
    /// Absolute or project-relative file paths the macro will read or edit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Named symbols (class, method, field) to anchor the macro.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    /// Byte or line ranges within a file, encoded as `"path:start-end"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<String>,
}

// ---------------------------------------------------------------------------
// MacroInvocation
// ---------------------------------------------------------------------------

/// A typed request to plan (and optionally apply) a macro.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroInvocation {
    /// Identifies the `MacroDefinition` to invoke.
    pub macro_id: String,
    /// Optional pinned version. `None` resolves the latest registered version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Absolute path to the project root. Used for path resolution and the
    /// registered-project safety check in `refactor::apply`.
    pub project_dir: String,
    /// Typed input values. Must conform to `MacroDefinition.inputs_schema`.
    pub inputs: serde_json::Map<String, Value>,
    /// Optional source anchors (files / symbols / ranges).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchors: Option<MacroAnchors>,
    /// Operator-supplied authority opt-out flags (e.g.
    /// `"acknowledge_public_api_change"`). Agents may thread these through
    /// from operator inputs but MUST NOT default or infer them (RX-V1).
    #[serde(default)]
    pub operator_opt_outs: Vec<String>,
}

// ---------------------------------------------------------------------------
// MacroSemanticStatus
// ---------------------------------------------------------------------------

/// Semantic verification tier for a `MacroPlan` or individual operation.
///
/// Reuses the refactor [`crate::refactor::SemanticStatus`] ladder and adds
/// two macro-only tiers:
///
/// | `MacroSemanticStatus` | Maps to `SemanticStatus`          | Notes |
/// |---|---|---|
/// | `syntax_only`          | `SemanticStatus::SyntaxOnly`       | Tree-sitter parse only |
/// | `indexed_hints`        | `SemanticStatus::IndexedHints`     | Project index, no LSP |
/// | `lsp_verified`         | `SemanticStatus::LspVerified`      | Full LSP round-trip |
/// | `lsp_verified_partial` | `SemanticStatus::LspVerifiedPartial` | LSP with caveats |
/// | `template_only`        | *(no refactor equivalent)*         | Generated text unverified |
/// | `mixed`                | *(no refactor equivalent)*         | Per-op statuses differ; worst-case when lowering |
///
/// A backend that requires an AST/LSP check and finds it unavailable
/// **fails closed** — it never silently downgrades to `template_only`
/// (mirrors RX-V3).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MacroSemanticStatus {
    SyntaxOnly,
    IndexedHints,
    LspVerified,
    LspVerifiedPartial,
    /// Generated text has not been parsed or LSP-verified. Weakest tier.
    TemplateOnly,
    /// Per-operation statuses differ. When lowering to `RefactorPlan`,
    /// the worst-case status across all operations must be used.
    Mixed,
}

// ---------------------------------------------------------------------------
// MacroOperation
// ---------------------------------------------------------------------------

/// A single step in a `MacroDefinition`'s operation list.
///
/// The set of operations is **fixed** — new synthesis forms require a new
/// variant, not a free-form eval hook. Backend-specific detail is captured
/// in opaque `serde_json::Value` fields that the relevant backend interprets
/// at plan time.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MacroOperation {
    /// Inspect project style, packages, symbols, DI conventions, or build
    /// commands. Binds to the code-nav/LSP query substrate; does not
    /// reimplement search. Result is captured into `name` for downstream ops.
    Probe {
        /// Named result slot; referenced by later operations.
        name: String,
        /// Nav substrate binding spec (backend-specific; opaque at this layer).
        /// Backend dispatch: `java.search.type`, `java.search.member`,
        /// `bbox_code_query`, `bbox_code_symbols`, `bbox_code_refs`, etc.
        spec: Value,
    },

    /// Generate a new artifact via a typed backend op (e.g. JavaPoet).
    /// Produces a `FileCreate` in the `EditSet`.
    Emit {
        /// Slot name identifying this output (e.g. `"interface_file"`).
        name: String,
        /// Backend op spec (JavaPoet, template, etc.); opaque at this layer.
        backend_op: Value,
    },

    /// Transform existing source via a backend (e.g. OpenRewrite).
    /// Produces `FileEdit`s (possibly full-span) in the `EditSet`.
    Rewrite {
        /// Files or symbols to rewrite.
        targets: Vec<String>,
        /// Backend op spec (OpenRewrite recipe, etc.); opaque at this layer.
        backend_op: Value,
    },

    /// Delegate to an existing `bbox_refactor_plan` primitive when that is the
    /// correct granular tool (e.g. `extract_java_class`).
    DelegateRefactor {
        /// `bbox_refactor_plan` kind string, e.g. `"extract_java_class"`.
        refactor_kind: String,
        /// Parameters forwarded to the refactor planner (matches
        /// `RefactorPlanParams` fields).
        params: Value,
    },

    /// Parse / organize imports / compile / test one or more targets.
    /// Produces a `MacroPlanCheck` entry and eventually a `ValidationStep` in
    /// the lowered `RefactorPlan`.
    Validate {
        /// Check kind: `"parse"`, `"organize_imports"`, `"compile"`, `"test"`,
        /// `"lsp_no_diagnostics"`.
        check: String,
        /// Paths or target names to validate.
        targets: Vec<String>,
    },

    /// Emit structured residue (follow-up notes, questions) without mutating
    /// any file. Captured in `MacroPlan.questions` or side-channel notes.
    Record {
        /// Short label for the note, e.g. `"manual_wiring_required"`.
        label: String,
        /// Note body; `${path}` interpolation supported via
        /// [`crate::macros::expr::interpolate`].
        body: String,
    },
}

// ---------------------------------------------------------------------------
// MacroProbe
// ---------------------------------------------------------------------------

/// A named probe slot declared in a `MacroDefinition`. Bound to the
/// code-nav/LSP query substrate at plan time; result stored in `Context.probes`
/// under `name`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroProbe {
    /// Slot name, e.g. `"caller_type"`, `"binding"`.
    pub name: String,
    /// Human-readable description of what this probe discovers.
    pub description: String,
    /// Nav substrate binding spec (same shape as `MacroOperation::Probe.spec`).
    pub spec: Value,
}

// ---------------------------------------------------------------------------
// MacroRefusal
// ---------------------------------------------------------------------------

/// A conditional refusal rule. When `when` evaluates to `true` against the
/// planning context, the macro refuses with `code` and `message`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroRefusal {
    /// Predicate that triggers the refusal when true.
    pub when: Predicate,
    /// Short machine-readable error code, e.g. `"error.type_already_exists"`.
    pub code: String,
    /// Human-readable message; `${path}` interpolation supported.
    pub message: String,
}

// ---------------------------------------------------------------------------
// MacroValidation
// ---------------------------------------------------------------------------

/// A post-apply validation rule in a `MacroDefinition`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroValidation {
    /// Check kind: `"parse"`, `"organize_imports"`, `"compile"`, `"test"`.
    pub check: String,
    /// Files or targets to validate.
    pub targets: Vec<String>,
    /// Optional guard; validation runs only when this predicate is true.
    /// When absent the validation always runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Predicate>,
}

// ---------------------------------------------------------------------------
// MacroRefusalHit
// ---------------------------------------------------------------------------

/// A refusal that fired during planning. Collected in `MacroPlan.refusals`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct MacroRefusalHit {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// MacroPlanOperation
// ---------------------------------------------------------------------------

/// Summary of one operation as it appears in the `MacroPlan` review artifact.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroPlanOperation {
    /// Operation kind string, mirrors the `MacroOperation` variant names.
    pub kind: String,
    /// Named slot this operation writes to (for `probe` and `emit` ops).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Verification tier achieved for this operation.
    pub semantic_status: MacroSemanticStatus,
    /// One-line human-readable description of what this op will do.
    pub summary: String,
}

// ---------------------------------------------------------------------------
// MacroPlanCheck
// ---------------------------------------------------------------------------

/// A post-apply check recorded in the `MacroPlan`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroPlanCheck {
    /// Check kind, e.g. `"parse"`, `"lsp_no_diagnostics"`.
    pub check: String,
    /// Paths or targets to validate.
    pub targets: Vec<String>,
}

// ---------------------------------------------------------------------------
// EditSet
// ---------------------------------------------------------------------------

/// The complete set of file mutations produced by a macro's backend operations.
///
/// `EditSet` is a pure data container — backends produce it, planners
/// aggregate it, `macro_apply` lowers it to a single `RefactorPlan` and
/// calls `refactor::apply()`. No file is ever written from `EditSet` directly.
///
/// ### Lowering to `RefactorPlan` (1:1 mapping)
///
/// | `EditSet` field      | `RefactorPlan` field        |
/// |---|---|
/// | `file_edits`         | `edits`                     |
/// | `file_creates`       | `file_creates`              |
/// | `file_moves`         | `file_moves`                |
/// | `backends_used`      | title / provenance metadata |
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct EditSet {
    /// Mutations to existing files. Each `FileEdit` carries a preimage SHA-256
    /// that `refactor::apply` verifies before writing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_edits: Vec<FileEdit>,
    /// New files to create. Maps to `RefactorPlan.file_creates`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_creates: Vec<FileCreate>,
    /// File rename/move operations. Maps to `RefactorPlan.file_moves`.
    ///
    /// Each `FileMove` carries a preimage SHA-256 that `refactor::apply`
    /// verifies before executing the move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_moves: Vec<FileMove>,
    /// Backend identifiers that contributed edits/creates/moves to this set,
    /// e.g. `["java_poet", "open_rewrite"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends_used: Vec<String>,
}

// ---------------------------------------------------------------------------
// MacroPlan
// ---------------------------------------------------------------------------

/// The review artifact produced by macro planning.
///
/// `MacroPlan` is **not** a `RefactorPlan`. It is a human/operator-readable
/// description of what the macro will do, which backends participated, and
/// which authority opt-outs were consumed. It lowers to a single
/// `RefactorPlan` at `macro_apply` time, which then passes through
/// `refactor::apply()` with all its guards intact.
///
/// Per RX-V1, `operator_opt_outs_used` must be recorded here AND on the
/// durable lowered `RefactorPlan` — not just on a summary.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MacroPlan {
    /// The macro that produced this plan.
    pub macro_id: String,
    /// One-paragraph human-readable description of what will be done.
    pub summary: String,
    /// Aggregate verification tier across all operations. When operations
    /// differ, this is `MacroSemanticStatus::Mixed`; the lowered
    /// `RefactorPlan.semantic_status` must use the worst-case single tier.
    pub semantic_status: MacroSemanticStatus,
    /// Per-operation summaries in planned execution order.
    pub operations: Vec<MacroPlanOperation>,
    /// All file mutations that will be applied. Lowers to `RefactorPlan`.
    pub edits: EditSet,
    /// Validation checks to run after applying.
    pub checks: Vec<MacroPlanCheck>,
    /// Open questions for the operator (ambiguous inputs, unresolvable probes).
    #[serde(default)]
    pub questions: Vec<String>,
    /// Refusals that fired. Non-empty means the macro cannot proceed.
    #[serde(default)]
    pub refusals: Vec<MacroRefusalHit>,
    /// Backend identifiers that participated in planning.
    pub backends_used: Vec<String>,
    /// RX-V1: operator-authority opt-out flags actually consumed during
    /// planning. Agents may pass these through from operator inputs; they must
    /// not be inferred. Preserved on the lowered `RefactorPlan` as well.
    #[serde(default)]
    pub operator_opt_outs_used: Vec<String>,
    /// Structured provenance (invocation fingerprint, timestamp, model, etc.).
    pub provenance: Value,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refactor::FileCreate;
    use serde_json::json;

    fn minimal_definition() -> MacroDefinition {
        MacroDefinition {
            id: "java.add_service_boundary".into(),
            version: "1".into(),
            language: "java".into(),
            scope: MacroScope::Builtin,
            title: "Add Java service boundary".into(),
            inputs_schema: json!({}),
            effects: vec!["creates_type".into()],
            authority_gates: vec![],
            probes: vec![],
            operations: vec![],
            validations: vec![],
            refusals: vec![],
        }
    }

    fn minimal_plan() -> MacroPlan {
        MacroPlan {
            macro_id: "java.add_service_boundary".into(),
            summary: "Creates PaymentService interface + implementation".into(),
            semantic_status: MacroSemanticStatus::TemplateOnly,
            operations: vec![MacroPlanOperation {
                kind: "emit".into(),
                name: Some("interface_file".into()),
                semantic_status: MacroSemanticStatus::TemplateOnly,
                summary: "Emit PaymentService.java".into(),
            }],
            edits: EditSet {
                file_edits: vec![],
                file_creates: vec![FileCreate {
                    path: "/repo/src/PaymentService.java".into(),
                    content: "public interface PaymentService {}".into(),
                }],
                file_moves: vec![],
                backends_used: vec!["java_poet".into()],
            },
            checks: vec![MacroPlanCheck {
                check: "parse".into(),
                targets: vec!["/repo/src/PaymentService.java".into()],
            }],
            questions: vec![],
            refusals: vec![],
            backends_used: vec!["java_poet".into()],
            operator_opt_outs_used: vec![],
            provenance: json!({"session": "test"}),
        }
    }

    // -----------------------------------------------------------------------
    // Serde round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn macro_definition_round_trip() {
        let def = minimal_definition();
        let json = serde_json::to_string_pretty(&def).expect("serialize MacroDefinition");
        let def2: MacroDefinition =
            serde_json::from_str(&json).expect("deserialize MacroDefinition");
        assert_eq!(def2.id, def.id);
        assert_eq!(def2.version, def.version);
        assert_eq!(def2.scope, def.scope);
        assert_eq!(def2.effects, def.effects);
    }

    #[test]
    fn macro_plan_round_trip() {
        let plan = minimal_plan();
        let json = serde_json::to_string_pretty(&plan).expect("serialize MacroPlan");
        let plan2: MacroPlan = serde_json::from_str(&json).expect("deserialize MacroPlan");
        assert_eq!(plan2.macro_id, plan.macro_id);
        assert_eq!(plan2.semantic_status, plan.semantic_status);
        assert_eq!(plan2.edits.file_creates.len(), 1);
        assert_eq!(
            plan2.edits.file_creates[0].path,
            "/repo/src/PaymentService.java"
        );
        assert_eq!(plan2.backends_used, vec!["java_poet"]);
    }

    #[test]
    fn macro_definition_with_operations_round_trip() {
        let mut def = minimal_definition();
        def.operations = vec![
            MacroOperation::Probe {
                name: "caller_type".into(),
                spec: json!({"kind": "java.search.type", "query": "PaymentCaller"}),
            },
            MacroOperation::Emit {
                name: "interface_file".into(),
                backend_op: json!({"backend": "java_poet", "class": "PaymentService"}),
            },
            MacroOperation::Rewrite {
                targets: vec!["src/Caller.java".into()],
                backend_op: json!({"recipe": "AddImport"}),
            },
            MacroOperation::DelegateRefactor {
                refactor_kind: "extract_java_class".into(),
                params: json!({"source": "src/Caller.java"}),
            },
            MacroOperation::Validate {
                check: "parse".into(),
                targets: vec!["src/PaymentService.java".into()],
            },
            MacroOperation::Record {
                label: "follow_up".into(),
                body: "Wire Guice binding for ${inputs.service_name}".into(),
            },
        ];
        let json = serde_json::to_string_pretty(&def).expect("serialize");
        let def2: MacroDefinition = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(def2.operations.len(), 6);
    }

    #[test]
    fn macro_refusal_hit_round_trip() {
        let hit = MacroRefusalHit {
            code: "error.type_already_exists".into(),
            message: "PaymentService already exists in src/".into(),
        };
        let json = serde_json::to_string(&hit).expect("serialize");
        let hit2: MacroRefusalHit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hit, hit2);
    }

    #[test]
    fn edit_set_lowering_fields_present() {
        // Verify the fields that map to RefactorPlan exist with the correct names.
        let es = EditSet {
            file_edits: vec![],
            file_creates: vec![FileCreate {
                path: "/repo/Foo.java".into(),
                content: "class Foo {}".into(),
            }],
            file_moves: vec![],
            backends_used: vec!["java_poet".into()],
        };
        let json = serde_json::to_value(&es).expect("to_value");
        // file_creates key must be present for lowering to RefactorPlan.file_creates
        assert!(json.get("file_creates").is_some());
        // file_edits is empty so it is omitted by skip_serializing_if
        assert!(json.get("file_edits").is_none());
    }

    #[test]
    fn macro_semantic_status_serde() {
        let statuses = [
            (MacroSemanticStatus::SyntaxOnly, "syntax_only"),
            (MacroSemanticStatus::IndexedHints, "indexed_hints"),
            (MacroSemanticStatus::LspVerified, "lsp_verified"),
            (
                MacroSemanticStatus::LspVerifiedPartial,
                "lsp_verified_partial",
            ),
            (MacroSemanticStatus::TemplateOnly, "template_only"),
            (MacroSemanticStatus::Mixed, "mixed"),
        ];
        for (status, expected_str) in &statuses {
            let json = serde_json::to_string(status).expect("serialize");
            assert_eq!(json, format!("\"{expected_str}\""));
            let back: MacroSemanticStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&back, status);
        }
    }

    #[test]
    fn edit_set_file_moves_round_trip() {
        use crate::refactor::FileMove;
        let es = EditSet {
            file_edits: vec![],
            file_creates: vec![],
            file_moves: vec![FileMove {
                source_path: "/repo/src/Old.java".into(),
                target_path: "/repo/src/New.java".into(),
                original_sha256: "deadbeef".into(),
            }],
            backends_used: vec!["open_rewrite".into()],
        };
        let json = serde_json::to_value(&es).expect("serialize");
        // file_moves present when non-empty
        assert!(json.get("file_moves").is_some());
        let es2: EditSet = serde_json::from_value(json).expect("deserialize");
        assert_eq!(es2.file_moves.len(), 1);
        assert_eq!(es2.file_moves[0].source_path, "/repo/src/Old.java");
        assert_eq!(es2.file_moves[0].target_path, "/repo/src/New.java");

        // When file_moves is empty it should be omitted in serialization
        let es_empty = EditSet::default();
        let json_empty = serde_json::to_value(&es_empty).expect("serialize empty");
        assert!(
            json_empty.get("file_moves").is_none(),
            "empty file_moves should be omitted"
        );
    }

    #[test]
    fn macro_scope_serde() {
        let scopes = [
            (MacroScope::Builtin, "builtin"),
            (MacroScope::User, "user"),
            (MacroScope::Project, "project"),
        ];
        for (scope, expected_str) in &scopes {
            let json = serde_json::to_string(scope).expect("serialize");
            assert_eq!(json, format!("\"{expected_str}\""));
            let back: MacroScope = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&back, scope);
        }
    }

    #[test]
    fn macro_refusal_with_predicate_round_trip() {
        use crate::macros::expr::Predicate;
        let refusal = MacroRefusal {
            when: Predicate::Exists {
                path: "binding.type_exists".into(),
            },
            code: "error.type_already_exists".into(),
            message: "Type ${inputs.service_name} already exists".into(),
        };
        let json = serde_json::to_string_pretty(&refusal).expect("serialize");
        let refusal2: MacroRefusal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(refusal2.code, refusal.code);
        assert_eq!(refusal2.message, refusal.message);
    }
}
