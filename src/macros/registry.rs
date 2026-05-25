//! Macro registry — resolves [`MacroDefinition`]s from three ordered scopes.
//!
//! # Scope precedence (highest to lowest)
//!
//! 1. **Project** — `<project_dir>/.bbox/macros/<id>.json`
//! 2. **User** — `$BLACKBOX_MACROS_DIR` / `~/.config/blackbox/macros/<id>.json`
//! 3. **Builtin** — shipped with Blackbox (empty for now)
//!
//! Overlapping `id`s are deduplicated: the highest-precedence scope wins.
//! Project macros are reviewed like source code. User macros live in operator
//! config. Builtins ship with the daemon.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::macros::expr::Predicate;
use crate::macros::model::{MacroDefinition, MacroOperation, MacroScope};
use crate::macros::probe::ProbeSpec;

// ---------------------------------------------------------------------------
// RegistryError
// ---------------------------------------------------------------------------

/// Typed error returned by registry operations.
#[derive(Debug)]
pub enum RegistryError {
    /// Filesystem I/O failure (read, write, create_dir, remove).
    Io(std::io::Error),
    /// JSON parse failure on a macro file or incoming definition.
    Parse(serde_json::Error),
    /// A macro with the same `id` + `version` already exists in project scope
    /// and `overwrite` was not set.
    Conflict { id: String, version: String },
    /// No macro with the given `id` was found for the target operation.
    NotFound { id: String },
    /// The definition failed structural validation (missing fields, bad
    /// predicates, etc.).
    InvalidDefinition(String),
    /// The `id` violates the allowed grammar `^[a-z0-9]+(?:[._-][a-z0-9]+)*$`.
    ///
    /// Rejected **before** any `PathBuf` join to prevent path traversal
    /// (e.g. `../../etc/x`, `a/b`, `..` all fail here).
    InvalidId(String),
    /// The path supplied as `project_dir` does not exist or is not a
    /// directory.
    NotAProjectDirectory(PathBuf),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Parse(e) => write!(f, "parse error: {e}"),
            Self::Conflict { id, version } => {
                write!(
                    f,
                    "macro '{id}' version '{version}' already exists in project scope (use overwrite=true to replace)"
                )
            }
            Self::NotFound { id } => write!(f, "macro not found: '{id}'"),
            Self::InvalidDefinition(msg) => write!(f, "invalid definition: {msg}"),
            Self::InvalidId(id) => write!(
                f,
                "invalid macro id '{id}': must match ^[a-z0-9]+(?:[._-][a-z0-9]+)*$ \
                 (lowercase alnum segments separated by single '.' '_' or '-'; \
                 no path separators, no consecutive separators)"
            ),
            Self::NotAProjectDirectory(p) => write!(f, "not a project directory: {}", p.display()),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<std::io::Error> for RegistryError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RegistryError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e)
    }
}

// ---------------------------------------------------------------------------
// Validation report types
// ---------------------------------------------------------------------------

/// A validation report for a single macro definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroValidationReport {
    /// True when there are zero issues.
    pub valid: bool,
    /// All discovered issues (warnings and errors).
    pub issues: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub severity: ValidationSeverity,
    /// Dot-path to the offending field, e.g. `"refusals[0].when"`.
    pub field: String,
    /// Human-readable description of the issue.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Error,
    Warning,
}

impl ValidationIssue {
    pub fn error(field: &str, message: &str) -> Self {
        Self {
            severity: ValidationSeverity::Error,
            field: field.to_string(),
            message: message.to_string(),
        }
    }

    #[allow(dead_code)]
    pub fn warning(field: &str, message: &str) -> Self {
        Self {
            severity: ValidationSeverity::Warning,
            field: field.to_string(),
            message: message.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// MacroRegistry
// ---------------------------------------------------------------------------

/// Maximum number of probe slots a single [`MacroDefinition`] may declare.
const MAX_PROBE_COUNT: usize = 32;

/// Stateless registry that resolves [`MacroDefinition`]s from three scopes.
///
/// Methods accept a `project_dir` parameter for project-scoped lookups.
/// When `project_dir` is `None`, only user + builtin scopes are searched.
pub struct MacroRegistry;

impl MacroRegistry {
    // -----------------------------------------------------------------------
    // Path helpers
    // -----------------------------------------------------------------------

    /// Directory for user-scoped macros.
    ///
    /// Resolved from `$BLACKBOX_MACROS_DIR` when set (and non-empty), otherwise
    /// `~/.config/blackbox/macros/` via the `dirs` crate. This mirrors the
    /// config-file resolution in `config.rs` (which uses
    /// `dirs::config_dir().map(|d| d.join("blackbox").join("config.toml"))`).
    fn user_macros_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("BLACKBOX_MACROS_DIR") {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir);
            }
        }
        dirs::config_dir()
            .map(|d| d.join("blackbox").join("macros"))
            .unwrap_or_else(|| {
                // Bare fallback: no XDG config dir available
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home)
                    .join(".config")
                    .join("blackbox")
                    .join("macros")
            })
    }

    /// Directory for project-scoped macros: `<project_dir>/.bbox/macros/`.
    fn project_macros_dir(project_dir: &Path) -> PathBuf {
        project_dir.join(".bbox").join("macros")
    }

    // -----------------------------------------------------------------------
    // Load helpers
    // -----------------------------------------------------------------------

    /// Builtin macro definitions — currently empty.
    fn builtin_definitions() -> Vec<MacroDefinition> {
        vec![]
    }

    /// Load every `.json` file in `dir` as a [`MacroDefinition`].
    ///
    /// Malformed files are logged with `tracing::warn!` and skipped — a bad
    /// macro file never prevents loading the rest.
    fn load_dir(dir: &Path) -> Vec<MacroDefinition> {
        if !dir.is_dir() {
            return vec![];
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return vec![],
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match Self::load_file(&path) {
                Ok(def) => out.push(def),
                Err(e) => {
                    tracing::warn!(path = %path.display(), "skipping macro file: {e}");
                }
            }
        }
        out
    }

    /// Read and validate a single macro file.
    fn load_file(path: &Path) -> Result<MacroDefinition, RegistryError> {
        let content = fs::read_to_string(path)?;
        let def: MacroDefinition = serde_json::from_str(&content)?;
        // Validate all predicates parse correctly
        Self::validate_predicates(&def).map_err(|issues| {
            RegistryError::InvalidDefinition(format!(
                "predicate validation failed ({} issue(s))",
                issues.len()
            ))
        })?;
        Ok(def)
    }

    /// Merge multiple scope groups into a single deduplicated list.
    ///
    /// Groups are ordered highest-precedence-first. Within a group, earlier
    /// files come first. When two definitions share an `id`, the one from the
    /// higher-precedence group wins.
    fn merge(groups: Vec<Vec<MacroDefinition>>) -> Vec<MacroDefinition> {
        let precedence: HashMap<MacroScope, u8> = [
            (MacroScope::Project, 2),
            (MacroScope::User, 1),
            (MacroScope::Builtin, 0),
        ]
        .into();

        let mut seen: HashMap<String, (u8, usize)> = HashMap::new();
        let mut out: Vec<MacroDefinition> = Vec::new();

        for group in groups {
            for def in group {
                let pr = precedence.get(&def.scope).copied().unwrap_or(0);
                match seen.get(&def.id) {
                    Some(&(existing_pr, _)) if existing_pr >= pr => continue,
                    _ => {}
                }
                if let Some(&(_, idx)) = seen.get(&def.id) {
                    out[idx] = def;
                } else {
                    seen.insert(def.id.clone(), (pr, out.len()));
                    out.push(def);
                }
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Id grammar
    // -----------------------------------------------------------------------

    /// Validate a macro id against the allowed grammar.
    ///
    /// Grammar: `^[a-z0-9]+(?:[._-][a-z0-9]+)*$`
    ///
    /// - Segments: one or more lowercase ASCII alphanumeric chars.
    /// - Separators: exactly one of `'.'`, `'_'`, or `'-'` between segments.
    /// - No path separators (`/`, `\`), no `..`, no consecutive separators,
    ///   no leading/trailing separator.
    ///
    /// Called **before** any `PathBuf` join to prevent path traversal.
    fn is_valid_macro_id(id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        let bytes = id.as_bytes();
        let is_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let is_sep = |b: u8| matches!(b, b'.' | b'_' | b'-');

        // Must start and end with an alnum character.
        if !is_alnum(bytes[0]) || !is_alnum(*bytes.last().unwrap()) {
            return false;
        }

        // Scan interior: valid chars only, no consecutive separators.
        let mut prev_sep = false;
        for &b in bytes {
            if is_alnum(b) {
                prev_sep = false;
            } else if is_sep(b) {
                if prev_sep {
                    return false; // consecutive separators
                }
                prev_sep = true;
            } else {
                return false; // invalid character (includes '/', '\', uppercase, etc.)
            }
        }
        true
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    /// Returns `true` if any string field within `spec` contains a literal `${`.
    ///
    /// Interpolation in probe/operation specs is not yet supported. A spec
    /// containing `${...}` would survive deserialization as a literal string and
    /// silently evaluate to false in any predicate — a dangerous footgun. Reject
    /// at registry time so authors receive a clear error rather than silent
    /// wrong-answer behaviour.
    fn spec_contains_interpolation(spec: &serde_json::Value) -> bool {
        match spec {
            serde_json::Value::String(s) => s.contains("${"),
            serde_json::Value::Array(arr) => arr.iter().any(Self::spec_contains_interpolation),
            serde_json::Value::Object(map) => map.values().any(Self::spec_contains_interpolation),
            _ => false,
        }
    }

    /// Check that all `Predicate` values in refusals are structurally valid.
    fn validate_predicates(def: &MacroDefinition) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for (i, refusal) in def.refusals.iter().enumerate() {
            // Round-trip through serde to confirm the predicate is well-formed
            if let Err(e) =
                serde_json::to_value(&refusal.when).and_then(serde_json::from_value::<Predicate>)
            {
                errors.push(format!("refusals[{i}].when: {e}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Structural definition validation (does NOT write anything).
    ///
    /// Checks: required fields (id, version, language, title) are non-empty,
    /// `inputs_schema` is a JSON object, all refusal predicates round-trip,
    /// operation kinds are known (checked by serde deserialization), and
    /// validation targets are non-empty.
    pub fn validate(def: &MacroDefinition) -> MacroValidationReport {
        let mut issues = Vec::new();

        if def.id.trim().is_empty() {
            issues.push(ValidationIssue::error(
                "id",
                "id is required and must be non-empty",
            ));
        } else if !Self::is_valid_macro_id(&def.id) {
            issues.push(ValidationIssue::error(
                "id",
                "id must match ^[a-z0-9]+(?:[._-][a-z0-9]+)*$ \
                 (lowercase alnum segments separated by single '.' '_' or '-'; \
                 no path separators, uppercase, or consecutive separators)",
            ));
        }
        if def.version.trim().is_empty() {
            issues.push(ValidationIssue::error(
                "version",
                "version is required and must be non-empty",
            ));
        }
        if def.language.trim().is_empty() {
            issues.push(ValidationIssue::error(
                "language",
                "language is required and must be non-empty",
            ));
        }
        if def.title.trim().is_empty() {
            issues.push(ValidationIssue::error(
                "title",
                "title is required and must be non-empty",
            ));
        }
        if !def.inputs_schema.is_object() {
            issues.push(ValidationIssue::error(
                "inputs_schema",
                "must be a JSON Schema object (type: object)",
            ));
        }

        for (i, refusal) in def.refusals.iter().enumerate() {
            let path = format!("refusals[{i}].when");
            match serde_json::to_value(&refusal.when).and_then(serde_json::from_value::<Predicate>)
            {
                Ok(_) => {}
                Err(e) => {
                    issues.push(ValidationIssue::error(
                        &path,
                        &format!("invalid predicate: {e}"),
                    ));
                }
            }
        }

        for (i, validation) in def.validations.iter().enumerate() {
            if validation.check.trim().is_empty() {
                issues.push(ValidationIssue::error(
                    &format!("validations[{i}].check"),
                    "check kind is required",
                ));
            }
            if validation.targets.is_empty() {
                issues.push(ValidationIssue::error(
                    &format!("validations[{i}].targets"),
                    "targets must not be empty",
                ));
            }
        }

        // Probe validation: cap count, reject duplicate names, validate spec kind.
        //
        // Inline MacroOperation::Probe entries share the same name namespace as
        // top-level def.probes (both populate Context.probes), so they are counted
        // and deduplicated together. The combined total must not exceed MAX_PROBE_COUNT.
        let inline_probe_count = def
            .operations
            .iter()
            .filter(|op| matches!(op, MacroOperation::Probe { .. }))
            .count();
        let total_probe_count = def.probes.len() + inline_probe_count;
        if total_probe_count > MAX_PROBE_COUNT {
            issues.push(ValidationIssue::error(
                "probes",
                &format!(
                    "macro declares {} probe(s) total ({} top-level + {} inline); \
                     maximum allowed is {MAX_PROBE_COUNT}",
                    total_probe_count,
                    def.probes.len(),
                    inline_probe_count
                ),
            ));
        }

        // Duplicate probe name check across BOTH top-level and inline probes.
        // Top-level and inline probe names share one namespace.
        let mut seen_probe_names: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for (i, probe) in def.probes.iter().enumerate() {
            if !seen_probe_names.insert(probe.name.as_str()) {
                issues.push(ValidationIssue::error(
                    &format!("probes[{i}].name"),
                    &format!("duplicate probe name '{}'", probe.name),
                ));
            }
            // Reject interpolation placeholders in probe specs (not yet supported).
            if Self::spec_contains_interpolation(&probe.spec) {
                issues.push(ValidationIssue::error(
                    &format!("probes[{i}].spec"),
                    "interpolation in probe/operation specs is not yet supported; remove ${...}",
                ));
            }
            // ProbeSpec deserialization: reject unknown kinds / malformed specs.
            match serde_json::from_value::<ProbeSpec>(probe.spec.clone()) {
                Ok(_) => {}
                Err(e) => {
                    issues.push(ValidationIssue::error(
                        &format!("probes[{i}].spec"),
                        &format!(
                            "invalid or unknown probe spec kind (bounded to code_query, \
                             code_symbols, workspace_symbol): {e}"
                        ),
                    ));
                }
            }
        }

        // Validate inline MacroOperation::Probe entries: same checks as top-level.
        for (i, op) in def.operations.iter().enumerate() {
            if let MacroOperation::Probe {
                name,
                spec: spec_val,
            } = op
            {
                if !seen_probe_names.insert(name.as_str()) {
                    issues.push(ValidationIssue::error(
                        &format!("operations[{i}].name"),
                        &format!(
                            "duplicate probe name '{}' (shared namespace with top-level probes)",
                            name
                        ),
                    ));
                }
                // Reject interpolation placeholders in inline probe specs.
                if Self::spec_contains_interpolation(spec_val) {
                    issues.push(ValidationIssue::error(
                        &format!("operations[{i}].spec"),
                        "interpolation in probe/operation specs is not yet supported; remove ${...}",
                    ));
                }
                match serde_json::from_value::<ProbeSpec>(spec_val.clone()) {
                    Ok(_) => {}
                    Err(e) => {
                        issues.push(ValidationIssue::error(
                            &format!("operations[{i}].spec"),
                            &format!(
                                "inline Probe operation '{}' has invalid or unknown spec kind \
                                 (bounded to code_query, code_symbols, workspace_symbol): {e}",
                                name
                            ),
                        ));
                    }
                }
            }
        }

        MacroValidationReport {
            valid: issues.is_empty(),
            issues,
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// List all macros from all available scopes, merged and deduplicated by
    /// `id`. When `project_dir` is `Some`, project-scoped macros are included
    /// at the highest precedence.
    pub fn list(project_dir: Option<&Path>) -> Vec<MacroDefinition> {
        let mut groups: Vec<Vec<MacroDefinition>> = Vec::new();

        // Highest precedence first — merge picks the first hit per id
        if let Some(pd) = project_dir {
            groups.push(Self::load_dir(&Self::project_macros_dir(pd)));
        }
        groups.push(Self::load_dir(&Self::user_macros_dir()));
        groups.push(Self::builtin_definitions());

        Self::merge(groups)
    }

    /// Look up a single macro by `id`. Returns `None` when no scope has a
    /// definition with that `id`.
    ///
    /// Rejects `id` values that violate the id grammar before any path
    /// construction — see [`RegistryError::InvalidId`].
    pub fn get(
        project_dir: Option<&Path>,
        id: &str,
    ) -> Result<Option<MacroDefinition>, RegistryError> {
        // Reject non-empty ids that violate the grammar before any path
        // construction (path traversal guard).
        if !id.is_empty() && !Self::is_valid_macro_id(id) {
            return Err(RegistryError::InvalidId(id.to_string()));
        }
        let all = Self::list(project_dir);
        Ok(all.into_iter().find(|m| m.id == id))
    }

    /// Register a macro in the **project scope** by writing it to
    /// `<project_dir>/.bbox/macros/<id>.json`.
    ///
    /// - Rejects `def.id` values that violate the id grammar **before** any
    ///   path join — see [`RegistryError::InvalidId`].
    /// - Creates the `.bbox/macros/` directory if it does not exist.
    /// - Validates the definition before writing.
    /// - When `overwrite` is `false` (default) and a file with the same `id`
    ///   already exists, returns [`RegistryError::Conflict`].
    /// - When `overwrite` is `true`, silently replaces the existing file.
    pub fn register(
        project_dir: &Path,
        def: MacroDefinition,
        overwrite: bool,
    ) -> Result<(), RegistryError> {
        // Reject non-empty ids that violate the grammar before any PathBuf join
        // (path traversal guard). Empty ids are caught by validate() below as a
        // required-field error.
        if !def.id.is_empty() && !Self::is_valid_macro_id(&def.id) {
            return Err(RegistryError::InvalidId(def.id.clone()));
        }

        if !project_dir.is_dir() {
            return Err(RegistryError::NotAProjectDirectory(
                project_dir.to_path_buf(),
            ));
        }

        let report = Self::validate(&def);
        if !report.valid {
            let details: Vec<String> = report
                .issues
                .iter()
                .map(|i| format!("  {}: {} — {}", i.severity_str(), i.field, i.message))
                .collect();
            return Err(RegistryError::InvalidDefinition(details.join("\n")));
        }

        let dir = Self::project_macros_dir(project_dir);
        fs::create_dir_all(&dir)?;

        let file_path = dir.join(format!("{}.json", def.id));

        if file_path.exists() && !overwrite {
            // A file with this id already exists — refuse unless overwrite
            return Err(RegistryError::Conflict {
                id: def.id.clone(),
                version: def.version.clone(),
            });
        }

        let json = serde_json::to_string_pretty(&def)?;
        fs::write(&file_path, json)?;
        Ok(())
    }

    /// Remove a project-scope macro by `id`.
    ///
    /// Rejects `id` values that violate the id grammar before any path
    /// construction — see [`RegistryError::InvalidId`].
    pub fn unregister(project_dir: &Path, id: &str) -> Result<(), RegistryError> {
        // Reject non-empty ids that violate the grammar before any path
        // construction (path traversal guard).
        if !id.is_empty() && !Self::is_valid_macro_id(id) {
            return Err(RegistryError::InvalidId(id.to_string()));
        }
        let dir = Self::project_macros_dir(project_dir);
        let file_path = dir.join(format!("{id}.json"));
        if !file_path.exists() {
            return Err(RegistryError::NotFound { id: id.into() });
        }
        fs::remove_file(&file_path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper on ValidationIssue
// ---------------------------------------------------------------------------

impl ValidationIssue {
    fn severity_str(&self) -> &'static str {
        match self.severity {
            ValidationSeverity::Error => "error",
            ValidationSeverity::Warning => "warning",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::model::MacroRefusal;
    use serde_json::json;
    use std::path::Path;

    fn example_def(scope: MacroScope) -> MacroDefinition {
        MacroDefinition {
            id: "test.macro".into(),
            version: "1.0.0".into(),
            language: "any".into(),
            scope,
            title: "Test Macro".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: vec![],
            operations: vec![],
            validations: vec![],
            refusals: vec![],
        }
    }

    fn example_def_with_predicate(scope: MacroScope) -> MacroDefinition {
        MacroDefinition {
            id: "test.predicate".into(),
            version: "1.0.0".into(),
            language: "any".into(),
            scope,
            title: "Predicate Test".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: vec![],
            operations: vec![],
            validations: vec![],
            refusals: vec![MacroRefusal {
                when: Predicate::Exists {
                    path: "inputs.foo".into(),
                },
                code: "error.foo".into(),
                message: "foo exists".into(),
            }],
        }
    }

    fn setup_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");
        // Create .bbox/macros dir so load_dir finds something
        std::fs::create_dir_all(project.join(".bbox").join("macros")).expect("create .bbox/macros");
        (dir, project)
    }

    // -----------------------------------------------------------------------
    // list / get from a tempdir
    // -----------------------------------------------------------------------

    #[test]
    fn list_empty_when_no_macros() {
        let (_dir, project) = setup_tempdir();
        let macros = MacroRegistry::list(Some(&project));
        assert!(
            macros.is_empty(),
            "expected no macros, got {}",
            macros.len()
        );
    }

    #[test]
    fn register_writes_file_and_list_returns_it() {
        let (_dir, project) = setup_tempdir();
        let def = example_def(MacroScope::Project);

        MacroRegistry::register(&project, def.clone(), false).expect("register");

        let macros = MacroRegistry::list(Some(&project));
        assert_eq!(macros.len(), 1);
        assert_eq!(macros[0].id, "test.macro");
        assert_eq!(macros[0].scope, MacroScope::Project);
    }

    #[test]
    fn get_returns_registered_macro() {
        let (_dir, project) = setup_tempdir();
        let def = example_def(MacroScope::Project);

        MacroRegistry::register(&project, def, false).expect("register");

        let found = MacroRegistry::get(Some(&project), "test.macro")
            .expect("get")
            .expect("macro should exist");
        assert_eq!(found.id, "test.macro");
        assert_eq!(found.version, "1.0.0");
    }

    #[test]
    fn get_returns_none_for_missing_id() {
        let (_dir, project) = setup_tempdir();
        let found = MacroRegistry::get(Some(&project), "nonexistent").expect("get");
        assert!(found.is_none(), "expected None for nonexistent macro");
    }

    // -----------------------------------------------------------------------
    // list-before-register: refuse duplicate id+version
    // -----------------------------------------------------------------------

    #[test]
    fn register_refuses_duplicate_id_without_overwrite() {
        let (_dir, project) = setup_tempdir();
        let def = example_def(MacroScope::Project);

        MacroRegistry::register(&project, def.clone(), false).expect("first register");

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::Conflict { id, version }) => {
                assert_eq!(id, "test.macro");
                assert_eq!(version, "1.0.0");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn register_overwrite_true_replaces_existing() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        MacroRegistry::register(&project, def.clone(), false).expect("first register");

        // Modify the definition
        def.title = "Updated Title".into();
        MacroRegistry::register(&project, def, true).expect("overwrite register");

        let found = MacroRegistry::get(Some(&project), "test.macro")
            .expect("get")
            .expect("macro should exist");
        assert_eq!(found.title, "Updated Title");
    }

    // -----------------------------------------------------------------------
    // unregister
    // -----------------------------------------------------------------------

    #[test]
    fn unregister_removes_macro() {
        let (_dir, project) = setup_tempdir();
        let def = example_def(MacroScope::Project);
        MacroRegistry::register(&project, def, false).expect("register");

        MacroRegistry::unregister(&project, "test.macro").expect("unregister");

        let macros = MacroRegistry::list(Some(&project));
        assert!(macros.is_empty(), "expected no macros after unregister");
    }

    #[test]
    fn unregister_nonexistent_returns_not_found() {
        let (_dir, project) = setup_tempdir();
        match MacroRegistry::unregister(&project, "nonexistent") {
            Err(RegistryError::NotFound { id }) => assert_eq!(id, "nonexistent"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // validation: malformed definitions rejected
    // -----------------------------------------------------------------------

    #[test]
    fn register_rejects_missing_id() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        def.id = "".into();

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::InvalidDefinition(msg)) => {
                assert!(msg.contains("id"), "error should mention id: {msg}");
            }
            other => panic!("expected InvalidDefinition, got {other:?}"),
        }
    }

    #[test]
    fn register_rejects_missing_version() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        def.version = "".into();

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::InvalidDefinition(msg)) => {
                assert!(
                    msg.contains("version"),
                    "error should mention version: {msg}"
                );
            }
            other => panic!("expected InvalidDefinition, got {other:?}"),
        }
    }

    #[test]
    fn register_rejects_missing_title() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        def.title = "".into();

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::InvalidDefinition(msg)) => {
                assert!(msg.contains("title"), "error should mention title: {msg}");
            }
            other => panic!("expected InvalidDefinition, got {other:?}"),
        }
    }

    #[test]
    fn register_rejects_missing_language() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        def.language = "".into();

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::InvalidDefinition(msg)) => {
                assert!(
                    msg.contains("language"),
                    "error should mention language: {msg}"
                );
            }
            other => panic!("expected InvalidDefinition, got {other:?}"),
        }
    }

    #[test]
    fn register_rejects_bad_inputs_schema() {
        let (_dir, project) = setup_tempdir();
        let mut def = example_def(MacroScope::Project);
        def.inputs_schema = json!("not_an_object");

        match MacroRegistry::register(&project, def, false) {
            Err(RegistryError::InvalidDefinition(msg)) => {
                assert!(
                    msg.contains("inputs_schema"),
                    "error should mention inputs_schema: {msg}"
                );
            }
            other => panic!("expected InvalidDefinition, got {other:?}"),
        }
    }

    #[test]
    fn register_rejects_non_project_directory() {
        let def = example_def(MacroScope::Project);
        match MacroRegistry::register(Path::new("/nonexistent/path"), def, false) {
            Err(RegistryError::NotAProjectDirectory(_)) => {} // expected
            other => panic!("expected NotAProjectDirectory, got {other:?}"),
        }
    }

    #[test]
    fn validate_reports_multiple_issues() {
        let mut def = example_def(MacroScope::Project);
        def.id = "".into();
        def.version = "".into();
        def.language = "".into();
        def.title = "".into();
        def.inputs_schema = json!("bad");

        let report = MacroRegistry::validate(&def);
        assert!(!report.valid);
        assert_eq!(report.issues.len(), 5); // 4 required fields + bad schema
    }

    #[test]
    fn load_skips_malformed_file_in_dir() {
        let (_dir, project) = setup_tempdir();
        let macros_dir = project.join(".bbox").join("macros");

        // Write a valid one
        let valid = example_def(MacroScope::Project);
        let valid_path = macros_dir.join("test.macro.json");
        std::fs::write(&valid_path, serde_json::to_string_pretty(&valid).unwrap()).unwrap();

        // Write a malformed one
        let bad_path = macros_dir.join("bad.json");
        std::fs::write(&bad_path, "not valid json").unwrap();

        // list should succeed, returning only the valid one
        let macros = MacroRegistry::list(Some(&project));
        assert_eq!(
            macros.len(),
            1,
            "expected 1 valid macro, got {}",
            macros.len()
        );
        assert_eq!(macros[0].id, "test.macro");
    }

    #[test]
    fn load_skips_non_json_files() {
        let (_dir, project) = setup_tempdir();
        let macros_dir = project.join(".bbox").join("macros");

        // Write a .txt file in the macros dir
        std::fs::write(macros_dir.join("readme.txt"), "not a macro").unwrap();

        let macros = MacroRegistry::list(Some(&project));
        assert!(macros.is_empty(), "non-.json files should be skipped");
    }

    #[test]
    fn load_skips_bad_predicate_in_refusal() {
        let (_dir, project) = setup_tempdir();
        let macros_dir = project.join(".bbox").join("macros");

        // Write a macro with an invalid predicate (using raw JSON)
        let bad_json = json!({
            "id": "bad.predicate",
            "version": "1",
            "language": "any",
            "scope": "project",
            "title": "Bad Predicate",
            "inputs_schema": {"type": "object"},
            "effects": [],
            "authority_gates": [],
            "probes": [],
            "operations": [],
            "validations": [],
            "refusals": [{
                "when": {"op": "nonexistent_operator", "path": "x"},
                "code": "E1",
                "message": "bad"
            }]
        });
        std::fs::write(
            macros_dir.join("bad.predicate.json"),
            serde_json::to_string_pretty(&bad_json).unwrap(),
        )
        .unwrap();

        // list should skip it (load_file validates predicates)
        let macros = MacroRegistry::list(Some(&project));
        assert!(macros.is_empty(), "bad predicate macro should be skipped");
    }

    // -----------------------------------------------------------------------
    // scope precedence on id collision
    // -----------------------------------------------------------------------

    #[test]
    fn project_scope_overrides_user_and_builtin() {
        let (_dir, project) = setup_tempdir();

        // Register a project-scope macro
        let mut project_def = example_def(MacroScope::Project);
        project_def.title = "Project Version".into();
        MacroRegistry::register(&project, project_def, false).expect("register project");

        // "Inject" a user-scope macro with the same id — in tests we can't
        // easily write to the real user dir, but we can test merging logic
        // directly via the merge function.
        let mut user_def = example_def(MacroScope::User);
        user_def.title = "User Version".into();

        let builtin_def = example_def(MacroScope::Builtin);

        let merged = MacroRegistry::merge(vec![
            MacroRegistry::load_dir(&MacroRegistry::project_macros_dir(&project)),
            vec![user_def],
            vec![builtin_def],
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].scope, MacroScope::Project);
        assert_eq!(merged[0].title, "Project Version");
    }

    #[test]
    fn user_scope_overrides_builtin() {
        let user_def = {
            let mut d = example_def(MacroScope::User);
            d.title = "User Version".into();
            d
        };
        let builtin_def = example_def(MacroScope::Builtin);

        let merged = MacroRegistry::merge(vec![
            vec![] as Vec<MacroDefinition>, // project (empty)
            vec![user_def],
            vec![builtin_def],
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].scope, MacroScope::User);
        assert_eq!(merged[0].title, "User Version");
    }

    #[test]
    fn distinct_ids_from_different_scopes_all_survive() {
        let mut p = example_def(MacroScope::Project);
        p.id = "project.only".into();
        let mut u = example_def(MacroScope::User);
        u.id = "user.only".into();
        let mut b = example_def(MacroScope::Builtin);
        b.id = "builtin.only".into();

        let merged = MacroRegistry::merge(vec![vec![p], vec![u], vec![b]]);
        // All three distinct ids should survive
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"project.only"));
        assert!(ids.contains(&"user.only"));
        assert!(ids.contains(&"builtin.only"));
    }

    // -----------------------------------------------------------------------
    // is_valid_macro_id — id grammar (path traversal guard)
    // -----------------------------------------------------------------------

    #[test]
    fn valid_ids_accepted() {
        let valid = [
            "java",
            "java2",
            "java.add-service_boundary",
            "test.macro",
            "a1b2c3",
            "x.y.z",
            "abc-def",
            "abc_def",
            "a0",
        ];
        for id in valid {
            assert!(
                MacroRegistry::is_valid_macro_id(id),
                "expected '{id}' to be valid"
            );
        }
    }

    #[test]
    fn path_traversal_ids_rejected() {
        // The three cases mandated by the task brief plus extras.
        let invalid = [
            "../../etc/x",
            "a/b",
            "..",
            ".",
            "",
            "../foo",
            "foo/../bar",
            "\\windows\\path",
            "foo\\bar",
            "Uppercase",
            "JAVA",
            "java..ops",
            "java--ops",
            "java__ops",
            ".java",
            "java.",
            "-java",
            "java-",
        ];
        for id in invalid {
            assert!(
                !MacroRegistry::is_valid_macro_id(id),
                "expected '{id}' to be invalid"
            );
        }
    }

    #[test]
    fn register_rejects_path_traversal_id() {
        let (_dir, project) = setup_tempdir();
        let traversal_ids = ["../../etc/x", "a/b", ".."];
        for bad_id in traversal_ids {
            let mut def = example_def(MacroScope::Project);
            def.id = bad_id.into();
            match MacroRegistry::register(&project, def, false) {
                Err(RegistryError::InvalidId(id)) => {
                    assert_eq!(id, bad_id, "InvalidId should carry the bad id");
                }
                other => panic!("expected InvalidId for '{bad_id}', got {other:?}"),
            }
        }
    }

    #[test]
    fn get_rejects_path_traversal_id() {
        let (_dir, project) = setup_tempdir();
        let traversal_ids = ["../../etc/x", "a/b", ".."];
        for bad_id in traversal_ids {
            match MacroRegistry::get(Some(&project), bad_id) {
                Err(RegistryError::InvalidId(id)) => {
                    assert_eq!(id, bad_id);
                }
                other => panic!("expected InvalidId for '{bad_id}', got {other:?}"),
            }
        }
    }

    #[test]
    fn unregister_rejects_path_traversal_id() {
        let (_dir, project) = setup_tempdir();
        let traversal_ids = ["../../etc/x", "a/b", ".."];
        for bad_id in traversal_ids {
            match MacroRegistry::unregister(&project, bad_id) {
                Err(RegistryError::InvalidId(id)) => {
                    assert_eq!(id, bad_id);
                }
                other => panic!("expected InvalidId for '{bad_id}', got {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // validate() standalone
    // -----------------------------------------------------------------------

    #[test]
    fn validate_accepts_valid_definition() {
        let def = example_def(MacroScope::Project);
        let report = MacroRegistry::validate(&def);
        assert!(
            report.valid,
            "valid definition should pass: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_accepts_definition_with_predicate() {
        let def = example_def_with_predicate(MacroScope::Project);
        let report = MacroRegistry::validate(&def);
        assert!(report.valid, "definition with valid predicate should pass");
    }

    // -----------------------------------------------------------------------
    // Probe validation (P4a)
    // -----------------------------------------------------------------------

    fn example_def_with_probes(probes: Vec<crate::macros::model::MacroProbe>) -> MacroDefinition {
        MacroDefinition {
            id: "probe.test".into(),
            version: "1".into(),
            language: "java".into(),
            scope: MacroScope::Project,
            title: "Probe Test".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes,
            operations: vec![],
            validations: vec![],
            refusals: vec![],
        }
    }

    #[test]
    fn validate_accepts_valid_probes() {
        use crate::macros::model::MacroProbe;
        let probes = vec![
            MacroProbe {
                name: "binding".into(),
                description: "Find class declarations".into(),
                spec: json!({
                    "kind": "code_query",
                    "file": "src/Foo.java",
                    "query": "(class_declaration) @c"
                }),
            },
            MacroProbe {
                name: "symbols".into(),
                description: "Find service symbols".into(),
                spec: json!({ "kind": "code_symbols", "query": "Service" }),
            },
        ];
        let def = example_def_with_probes(probes);
        let report = MacroRegistry::validate(&def);
        assert!(
            report.valid,
            "valid probes should pass: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_duplicate_probe_names() {
        use crate::macros::model::MacroProbe;
        let probe = MacroProbe {
            name: "binding".into(),
            description: "dup".into(),
            spec: json!({ "kind": "code_symbols" }),
        };
        let def = example_def_with_probes(vec![probe.clone(), probe]);
        let report = MacroRegistry::validate(&def);
        assert!(!report.valid, "duplicate probe names should fail");
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.message.contains("duplicate probe name")),
            "issues should mention duplicate name: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_bad_probe_spec_kind() {
        use crate::macros::model::MacroProbe;
        let probe = MacroProbe {
            name: "bad".into(),
            description: "bad kind".into(),
            spec: json!({ "kind": "find_usages", "symbol": "Foo" }),
        };
        let def = example_def_with_probes(vec![probe]);
        let report = MacroRegistry::validate(&def);
        assert!(!report.valid, "bad probe spec kind should fail");
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.field.contains("probes") && i.field.contains("spec")),
            "issues should mention probe spec: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_malformed_probe_spec() {
        use crate::macros::model::MacroProbe;
        let probe = MacroProbe {
            name: "bad".into(),
            description: "missing file".into(),
            // code_query requires "file" and "query"
            spec: json!({ "kind": "code_query" }),
        };
        let def = example_def_with_probes(vec![probe]);
        let report = MacroRegistry::validate(&def);
        assert!(!report.valid, "malformed probe spec should fail");
    }

    #[test]
    fn validate_rejects_too_many_probes() {
        use crate::macros::model::MacroProbe;
        let probes: Vec<MacroProbe> = (0..=MAX_PROBE_COUNT)
            .map(|i| MacroProbe {
                name: format!("probe{i}"),
                description: "x".into(),
                spec: json!({ "kind": "code_symbols" }),
            })
            .collect();
        let def = example_def_with_probes(probes);
        let report = MacroRegistry::validate(&def);
        assert!(!report.valid, "too many probes should fail");
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.field == "probes" && i.message.contains("maximum")),
            "issues should mention probe count: {:?}",
            report.issues
        );
    }

    // -----------------------------------------------------------------------
    // FIX 2: inline MacroOperation::Probe validation
    // -----------------------------------------------------------------------

    fn example_def_with_ops(operations: Vec<MacroOperation>) -> MacroDefinition {
        MacroDefinition {
            id: "inline.probe.test".into(),
            version: "1".into(),
            language: "java".into(),
            scope: MacroScope::Project,
            title: "Inline Probe Test".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: vec![],
            operations,
            validations: vec![],
            refusals: vec![],
        }
    }

    #[test]
    fn validate_accepts_valid_inline_probe_op() {
        let ops = vec![MacroOperation::Probe {
            name: "inline_sym".into(),
            spec: json!({ "kind": "code_symbols" }),
        }];
        let def = example_def_with_ops(ops);
        let report = MacroRegistry::validate(&def);
        assert!(
            report.valid,
            "valid inline probe op should pass: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_inline_probe_exceeds_budget() {
        use crate::macros::model::MacroProbe;
        // MAX_PROBE_COUNT top-level + 1 inline = over budget
        let mut def = MacroDefinition {
            id: "inline.probe.test".into(),
            version: "1".into(),
            language: "java".into(),
            scope: MacroScope::Project,
            title: "Inline Probe Over Budget".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: (0..MAX_PROBE_COUNT)
                .map(|i| MacroProbe {
                    name: format!("probe{i}"),
                    description: "x".into(),
                    spec: json!({ "kind": "code_symbols" }),
                })
                .collect(),
            operations: vec![MacroOperation::Probe {
                name: "inline_extra".into(),
                spec: json!({ "kind": "code_symbols" }),
            }],
            validations: vec![],
            refusals: vec![],
        };
        // Ensure we don't accidentally exceed the budget with the top-level probes alone
        def.probes.truncate(MAX_PROBE_COUNT); // exactly at limit
        let report = MacroRegistry::validate(&def);
        assert!(
            !report.valid,
            "MAX_PROBE_COUNT top-level + 1 inline must exceed budget: {:?}",
            report.issues
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.field == "probes" && i.message.contains("maximum")),
            "issues should mention probe count: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_duplicate_probe_name_across_top_level_and_inline() {
        use crate::macros::model::MacroProbe;
        // top-level probe "binding" and inline Probe op also named "binding" → collision
        let def = MacroDefinition {
            id: "dup.cross.test".into(),
            version: "1".into(),
            language: "java".into(),
            scope: MacroScope::Project,
            title: "Cross-set Dup".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: vec![MacroProbe {
                name: "binding".into(),
                description: "top-level".into(),
                spec: json!({ "kind": "code_symbols" }),
            }],
            operations: vec![MacroOperation::Probe {
                name: "binding".into(), // collision!
                spec: json!({ "kind": "code_symbols" }),
            }],
            validations: vec![],
            refusals: vec![],
        };
        let report = MacroRegistry::validate(&def);
        assert!(
            !report.valid,
            "duplicate name across top-level and inline probes must fail: {:?}",
            report.issues
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.message.contains("duplicate probe name")
                    || i.message.contains("shared namespace")),
            "issues should mention duplicate name: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_bad_inline_probe_spec_kind() {
        let ops = vec![MacroOperation::Probe {
            name: "bad_inline".into(),
            spec: json!({ "kind": "find_usages", "symbol": "Foo" }),
        }];
        let def = example_def_with_ops(ops);
        let report = MacroRegistry::validate(&def);
        assert!(
            !report.valid,
            "bad inline probe spec kind must fail: {:?}",
            report.issues
        );
        assert!(
            report.issues.iter().any(|i| i.field.contains("operations")
                && i.message.contains("invalid or unknown")
                && i.message.contains("spec kind")),
            "issues should mention inline probe spec: {:?}",
            report.issues
        );
    }

    // -----------------------------------------------------------------------
    // FIX 4: interpolation placeholders rejected in probe/op specs
    // -----------------------------------------------------------------------

    #[test]
    fn validate_rejects_interpolation_in_top_level_probe_spec() {
        use crate::macros::model::MacroProbe;
        let probe = MacroProbe {
            name: "sym".into(),
            description: "has interpolation".into(),
            spec: json!({
                "kind": "code_query",
                "file": "${inputs.file}",  // interpolation not supported
                "query": "(class_declaration) @c"
            }),
        };
        let def = example_def_with_probes(vec![probe]);
        let report = MacroRegistry::validate(&def);
        assert!(
            !report.valid,
            "probe spec with interpolation must be rejected: {:?}",
            report.issues
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.message.contains("interpolation")
                    || i.message.contains("${")
                    || i.message.contains("not yet supported")),
            "error should mention interpolation: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_rejects_interpolation_in_inline_probe_spec() {
        let ops = vec![MacroOperation::Probe {
            name: "inline_sym".into(),
            spec: json!({
                "kind": "workspace_symbol",
                "query": "${inputs.class_name}"  // interpolation not supported
            }),
        }];
        let def = example_def_with_ops(ops);
        let report = MacroRegistry::validate(&def);
        assert!(
            !report.valid,
            "inline probe spec with interpolation must be rejected: {:?}",
            report.issues
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.message.contains("interpolation")
                    || i.message.contains("not yet supported")),
            "error should mention interpolation: {:?}",
            report.issues
        );
    }

    #[test]
    fn validate_accepts_spec_without_interpolation() {
        use crate::macros::model::MacroProbe;
        let probe = MacroProbe {
            name: "sym".into(),
            description: "clean spec".into(),
            spec: json!({
                "kind": "workspace_symbol",
                "query": "PaymentService"  // no ${...}
            }),
        };
        let def = example_def_with_probes(vec![probe]);
        let report = MacroRegistry::validate(&def);
        assert!(
            report.valid,
            "spec without interpolation must pass: {:?}",
            report.issues
        );
    }
}
